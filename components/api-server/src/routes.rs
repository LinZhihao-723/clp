use axum::Json;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Sse;
use axum::response::sse::Event;
use axum::response::sse::KeepAlive;
use axum::routing::get;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::types::ArchiveId;
use futures::Stream;
use futures::StreamExt;
use search_result_listener::SearchResult;
use search_result_listener::SessionOutcome;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tower_http::cors::Any;
use tower_http::cors::CorsLayer;
use utoipa::IntoParams;
use utoipa::OpenApi;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::client::Client;
use crate::client::ClientError;
use crate::client::CompressionUsage;
use crate::client::CompressionUsageParams;
use crate::client::QueryConfig;
use crate::client::ValidatedCompressionUsageParams;
use crate::streaming_search::QueryJobTable;
use crate::streaming_search::SearchEvent;
use crate::streaming_search::StreamingSearch;

/// Factory method to create an Axum router configured with all API routes.
///
/// # Returns
///
/// A newly created [`axum::Router`] instance configured with all application routes.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`OpenApi::to_json`]'s return values on failure.
pub fn from_client(client: Client) -> Result<axum::Router, serde_json::Error> {
    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .route("/", get(health))
        .routes(routes!(health))
        .routes(routes!(query))
        .routes(routes!(stream_query))
        .routes(routes!(query_results))
        .routes(routes!(cancel_query))
        .routes(routes!(compression_usage))
        .route(
            "/column_metadata/{dataset_name}/timestamp",
            get(get_timestamp_column_names),
        )
        .with_state(client)
        .split_for_parts();
    let api_json = api.to_json()?;
    let router = router
        .route(
            "/openapi.json",
            get(|| async { (StatusCode::OK, api_json) }),
        )
        .layer(CorsLayer::new().allow_origin(Any));
    Ok(router)
}

// `utoipa::OpenApi` triggers `clippy::needless_for_each`
#[allow(clippy::needless_for_each)]
mod api_doc {
    // Using `super::...` can cause `super` to appear as a tag in the generated OpenAPI
    // documentation. Importing the paths directly prevents this issue.
    use super::__path_cancel_query;
    use super::__path_compression_usage;
    use super::__path_health;
    use super::__path_query;
    use super::__path_query_results;
    use super::__path_stream_query;
    use super::CompressionUsage;
    use super::QueryJobStatus;
    use super::StreamingSearchEnd;
    use super::StreamingSearchError;
    use super::StreamingSearchJob;
    use super::StreamingSearchResult;
    use crate::client::CompressionJobStatus;

    #[derive(utoipa::OpenApi)]
    #[openapi(
        info(
            title = "API Server",
            description = "API Server for CLP",
            contact(name = "YScope")
        ),
        paths(
            health,
            query,
            stream_query,
            query_results,
            cancel_query,
            compression_usage
        ),
        components(schemas(
            CompressionUsage,
            CompressionJobStatus,
            QueryJobStatus,
            StreamingSearchEnd,
            StreamingSearchError,
            StreamingSearchJob,
            StreamingSearchResult
        ))
    )]
    pub struct ApiDoc;
}
pub use api_doc::*;

#[utoipa::path(
    get,
    path = "/health",
    responses((status = OK, body = String))
)]
async fn health() -> String {
    "API server is running".to_owned()
}

#[utoipa::path(
    post,
    path = "/query",
    description = "Submits a new query job.",
    request_body(
        content= QueryConfig,
        example = json!({
            "query_string": "*",
            "datasets": ["default"],
            "time_range_begin_millisecs": 0,
            "time_range_end_millisecs": 17_356_896,
            "ignore_case": true,
            "max_num_results": 0,
            "buffer_results_in_mongodb": true,
            "count_by_time_bucket_size_millisecs": null
        })),
    responses(
        (
            status = OK,
            body = QueryResultsUri,
            description = "The URI to fetch the results of the submitted query.",
            example = json!({"query_results_uri":"query_results/1"})
        ),
        (status = INTERNAL_SERVER_ERROR)
    )
)]
async fn query(
    State(client): State<Client>,
    Json(query_config): Json<QueryConfig>,
) -> Result<Json<QueryResultsUri>, HandlerError> {
    tracing::info!("Submitting query: {:?}", query_config);
    let search_job_id = match client.submit_query(query_config).await {
        Ok(id) => {
            tracing::info!("Submitted query with search job ID: {}", id);
            id
        }
        Err(err) => {
            tracing::error!("Failed to submit query: {:?}", err);
            return Err(err.into());
        }
    };
    let uri = format!("query_results/{search_job_id}");
    Ok(Json(QueryResultsUri {
        query_results_uri: uri,
    }))
}

#[derive(Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct QueryResultsUri {
    /// The uri to get the query results.
    query_results_uri: String,
}

#[utoipa::path(
    post,
    path = "/query/stream",
    description = "Submits a new search job and streams its results back as Server-Sent Events \
        (SSE) in the same response. Only available when the package runs queries on Spider. If \
        the client disconnects before the job terminates, the job is marked as cancelling.",
    request_body(
        content = QueryConfig,
        example = json!({
            "query_string": "*",
            "datasets": ["default"],
            "time_range_begin_millisecs": 0,
            "time_range_end_millisecs": 17_356_896,
            "ignore_case": true
        })),
    responses(
        (
            status = OK,
            body = String,
            content_type = "text/event-stream",
            description = "Server-Sent Events stream of the search: first a `job` event whose \
                data is a `StreamingSearchJob`, then one unnamed event per result whose data is a \
                `StreamingSearchResult`, and finally an `end` event whose data is a \
                `StreamingSearchEnd`, sent once the job has terminated. If the job's status can't \
                be tracked, an `error` event whose data is a `StreamingSearchError` replaces the \
                `end` event.",
            example = "event: job\ndata: {\"query_job_id\":1}\n\ndata: \
                {\"archive_id\":\"018e90e5-8b2a-4a61-a2fc-cac799936caf\",\"timestamp\":\
                1633036800000,\"message\":\"Example log message\"}\n\nevent: end\ndata: \
                {\"status\":\"Succeeded\",\"num_results_emitted\":1,\
                \"num_duplicates_dropped\":0,\"num_protocol_errors\":0}\n\n"
        ),
        (
            status = BAD_REQUEST,
            description = "The query config is invalid, or sets an option that streaming search \
                doesn't support: a nonzero `max_num_results`, `buffer_results_in_mongodb`, or \
                `count_by_time_bucket_size_millisecs`."
        ),
        (
            status = NOT_IMPLEMENTED,
            description = "The package doesn't run queries on Spider."
        ),
        (status = INTERNAL_SERVER_ERROR)
    )
)]
async fn stream_query(
    State(client): State<Client>,
    Json(query_config): Json<QueryConfig>,
) -> Result<Sse<impl Stream<Item = Result<Event, HandlerError>>>, HandlerError> {
    serve_streaming_search(client.streaming_search(), query_config).await
}

/// The data of a streaming search's `job` event.
#[derive(Serialize, ToSchema)]
struct StreamingSearchJob {
    /// The ID of the search's query job.
    #[schema(value_type = i32)]
    query_job_id: QueryJobId,
}

/// The data of a streaming search's result event.
#[derive(Serialize, ToSchema)]
struct StreamingSearchResult {
    /// The ID of the archive that contains the result.
    #[schema(value_type = String)]
    archive_id: ArchiveId,

    /// The result's timestamp (epoch milliseconds).
    timestamp: i64,

    /// The result's log message, without its trailing newline.
    message: String,
}

impl From<SearchResult> for StreamingSearchResult {
    fn from(result: SearchResult) -> Self {
        let mut message = result.message;
        if message.ends_with('\n') {
            message.pop();
        }
        Self {
            archive_id: result.archive_id,
            timestamp: result.timestamp,
            message,
        }
    }
}

/// The data of a streaming search's `end` event: the terminal status of the search's query job,
/// and the search's statistics.
#[derive(Serialize, ToSchema)]
struct StreamingSearchEnd {
    /// The terminal status of the search's query job.
    status: QueryJobStatus,

    /// The number of results streamed.
    num_results_emitted: u64,

    /// The number of results dropped because a retried search task had already streamed them.
    num_duplicates_dropped: u64,

    /// The number of connections from search tasks closed because they violated the wire protocol.
    num_protocol_errors: u64,
}

impl From<SessionOutcome> for StreamingSearchEnd {
    fn from(outcome: SessionOutcome) -> Self {
        Self {
            status: outcome.status,
            num_results_emitted: outcome.stats.num_results_emitted,
            num_duplicates_dropped: outcome.stats.num_duplicates_dropped,
            num_protocol_errors: outcome.stats.num_protocol_errors,
        }
    }
}

/// The data of a streaming search's `error` event.
#[derive(Serialize, ToSchema)]
struct StreamingSearchError {
    /// A description of the error.
    message: String,
}

/// Submits a streaming search, and streams its events back as Server-Sent Events.
///
/// # Type Parameters
///
/// * `QueryJobTableType` - The query jobs table that the search is submitted to.
///
/// # Returns
///
/// The SSE response on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`HandlerError::NotImplemented`] if `streaming_search` is `None`, i.e., the package doesn't
///   run queries on Spider.
/// * Forwards [`StreamingSearch::submit`]'s return values on failure.
/// * Forwards [`Event::json_data`]'s return values on failure.
async fn serve_streaming_search<QueryJobTableType: QueryJobTable>(
    streaming_search: Option<&StreamingSearch<QueryJobTableType>>,
    query_config: QueryConfig,
) -> Result<
    Sse<impl Stream<Item = Result<Event, HandlerError>> + use<QueryJobTableType>>,
    HandlerError,
> {
    const END_EVENT: &str = "end";
    const ERROR_EVENT: &str = "error";
    const JOB_EVENT: &str = "job";

    tracing::debug!(
        query_config = ? query_config,
        "Received a streaming search request."
    );
    let Some(streaming_search) = streaming_search else {
        return Err(HandlerError::NotImplemented(
            "streaming search requires the package to run queries on Spider".to_owned(),
        ));
    };
    let search_stream = streaming_search
        .submit(query_config)
        .await
        .inspect_err(|e| tracing::error!(error = % e, "Failed to submit a streaming search."))?;
    let query_job_id = search_stream.query_job_id();

    let job_event = Event::default()
        .event(JOB_EVENT)
        .json_data(StreamingSearchJob { query_job_id })?;
    let mut has_streamed_result = false;
    let events = search_stream.map(move |search_event| match search_event {
        SearchEvent::Result(result) => {
            if !has_streamed_result {
                has_streamed_result = true;
                tracing::debug!(
                    query_job_id,
                    "Yielding the streaming search's first result."
                );
            }
            Ok(Event::default().json_data(StreamingSearchResult::from(result))?)
        }
        SearchEvent::End(Ok(outcome)) => Ok(Event::default()
            .event(END_EVENT)
            .json_data(StreamingSearchEnd::from(outcome))?),
        SearchEvent::End(Err(e)) => {
            tracing::error!(
                query_job_id,
                error = % e,
                "Failed to wait for the streaming search's query job to terminate."
            );
            Ok(Event::default()
                .event(ERROR_EVENT)
                .json_data(StreamingSearchError {
                    message: "failed to wait for the query job to terminate".to_owned(),
                })?)
        }
    });
    Ok(
        Sse::new(futures::stream::once(std::future::ready(Ok(job_event))).chain(events))
            .keep_alive(KeepAlive::default()),
    )
}

/// Query parameters for the query results endpoint.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct QueryResultsParams {
    /// When `true`, each SSE event contains the raw result document from the results cache
    /// serialized as JSON (including metadata such as the timestamp and the original file path);
    /// otherwise, each event contains only the log message. Only applies to query jobs whose
    /// results are buffered in `MongoDB`.
    #[serde(default)]
    raw_docs: bool,

    /// When `true`, results buffered in `MongoDB` are streamed sorted by timestamp descending;
    /// otherwise, they're streamed in insertion order. Only applies to query jobs whose results
    /// are buffered in `MongoDB`.
    #[serde(default)]
    sorted: bool,
}

#[utoipa::path(
    get,
    path = "/query_results/{search_job_id}",
    description = "Streams the results of a previously submitted query as Server-Sent Events \
        (SSE).",
    params(QueryResultsParams),
    responses(
        (
            status = OK,
            body = String,
            content_type = "text/event-stream",
            description = "Server-Sent Events stream of query results. Each event contains a \
                single line of the query result in JSON format.",
            example = r#"data: {"timestamp": 1633036800, "message": "Example log message"}\n"#
        ),
        (status = INTERNAL_SERVER_ERROR)
    )
)]
async fn query_results(
    State(client): State<Client>,
    Path(search_job_id): Path<u64>,
    Query(params): Query<QueryResultsParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, HandlerError>>>, HandlerError> {
    tracing::info!("Fetching results for search job ID: {}", search_job_id);
    let results_stream = match client
        .fetch_results(search_job_id, params.raw_docs, params.sorted)
        .await
    {
        Ok(stream) => {
            tracing::info!(
                "Successfully initiated result stream for search job ID {}",
                search_job_id
            );
            stream
        }
        Err(err) => {
            tracing::error!(
                "Failed to fetch results for search job ID {}: {:?}",
                search_job_id,
                err
            );
            return Err(err.into());
        }
    };
    let event_stream = results_stream.map(|res| {
        let message = res?;
        let trimmed_message = message.trim();
        if trimmed_message.lines().count() != 1 {
            tracing::error!("Received malformed log line:\n{}", trimmed_message);
            return Err(HandlerError::InternalServer);
        }
        Ok(Event::default().data(trimmed_message))
    });
    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

#[utoipa::path(
    delete,
    path = "/query/{search_job_id}",
    description = "Cancels a previously submitted query job.",
    responses(
        (
            status = OK,
            description = "The cancellation request was submitted successfully."
        ),
        (
            status = NOT_FOUND,
            description = "No cancellable query job with the given ID was found."
        ),
        (status = INTERNAL_SERVER_ERROR)
    )
)]
async fn cancel_query(
    State(client): State<Client>,
    Path(search_job_id): Path<u64>,
) -> Result<StatusCode, HandlerError> {
    tracing::info!("Cancelling search job ID: {}", search_job_id);
    match client.cancel_search_job(search_job_id).await {
        Ok(()) => {
            tracing::info!(
                "Successfully submitted cancellation request for search job ID: {}",
                search_job_id
            );
            Ok(StatusCode::OK)
        }
        Err(err) => {
            tracing::error!(
                "Failed to cancel search job ID {}: {:?}",
                search_job_id,
                err
            );
            Err(err.into())
        }
    }
}

async fn get_timestamp_column_names(
    State(client): State<Client>,
    Path(dataset_name): Path<String>,
) -> Result<Json<Vec<String>>, HandlerError> {
    let names = client
        .get_timestamp_column_names(&dataset_name)
        .await
        .map_err(|err| {
            tracing::error!(
                "Failed to get timestamp column names for dataset '{}': {:?}",
                dataset_name,
                err
            );
            HandlerError::from(err)
        })?;
    Ok(Json(names))
}

#[utoipa::path(
    get,
    path = "/usage/compression",
    description = "Gets resource usage statistics for compression jobs \
        within the given time range.",
    params(CompressionUsageParams),
    responses(
        (status = OK, body = Vec<CompressionUsage>),
        (status = BAD_REQUEST, description = "Invalid query parameters \
            (e.g., time_range_begin_millisecs > time_range_end_millisecs, \
            missing required fields)"),
        (status = INTERNAL_SERVER_ERROR)
    )
)]
async fn compression_usage(
    State(client): State<Client>,
    Query(params): Query<CompressionUsageParams>,
) -> Result<Json<Vec<CompressionUsage>>, HandlerError> {
    let validated = ValidatedCompressionUsageParams::try_from(params)?;
    tracing::info!(
        "Fetching compression usage: begin={}, end={}, job_statuses={:?}",
        validated.time_range_begin.timestamp_millis(),
        validated.time_range_end.timestamp_millis(),
        validated.job_statuses,
    );
    Ok(Json(
        client
            .get_compression_usage(&validated)
            .await
            .inspect_err(|err| {
                tracing::error!("Failed to fetch compression usage: {:?}", err);
            })?,
    ))
}

/// Generic errors for request handlers.
#[derive(Error, Debug)]
enum HandlerError {
    #[error("Internal server error")]
    InternalServer,
    #[error("Not found")]
    NotFound,
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Not implemented: {0}")]
    NotImplemented(String),
}

impl From<axum::Error> for HandlerError {
    fn from(_: axum::Error) -> Self {
        Self::InternalServer
    }
}

impl From<ClientError> for HandlerError {
    fn from(err: ClientError) -> Self {
        match err {
            ClientError::SearchJobNotFound(_) | ClientError::DatasetNotFound(_) => Self::NotFound,
            ClientError::InvalidDatasetName | ClientError::InvalidInput(_) => {
                Self::BadRequest(format!("{err}"))
            }
            _ => Self::InternalServer,
        }
    }
}

/// Converts [`HandlerError`] into an HTTP response.
impl IntoResponse for HandlerError {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::InternalServer => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            Self::NotImplemented(msg) => (StatusCode::NOT_IMPLEMENTED, msg).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    /// Builds a minimal Axum app that validates compression usage params via
    /// [`TryFrom<CompressionUsageParams>`] and returns the resolved
    /// status integer codes on success. No real database is needed.
    fn test_app() -> axum::Router {
        axum::Router::new().route(
            "/usage/compression",
            get(|Query(params): Query<CompressionUsageParams>| async move {
                let validated = ValidatedCompressionUsageParams::try_from(params)?;
                let codes: Vec<i32> = validated.job_statuses.into_iter().map(i32::from).collect();
                Ok::<_, HandlerError>(axum::Json(codes))
            }),
        )
    }

    async fn get_body(response: axum::response::Response) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("failed to read body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("body is not utf-8")
    }

    #[tokio::test]
    async fn reject_begin_greater_than_end() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=200&\
                         time_range_end_millisecs=100",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(body.contains("time_range_begin_millisecs must be <= time_range_end_millisecs"));
    }

    #[tokio::test]
    async fn reject_unknown_job_status() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=UNKNOWN",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(body.contains("Unknown job_status: UNKNOWN"));
    }

    #[tokio::test]
    async fn accept_lowercase_job_status() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=succeeded",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        assert_eq!(body, "[2]"); // succeeded → 2
    }

    #[tokio::test]
    async fn accept_valid_params_with_defaults() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        assert_eq!(body, "[2,3,4]"); // SUCCEEDED, FAILED, KILLED
    }

    #[tokio::test]
    async fn accept_comma_separated_job_status() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=SUCCEEDED,RUNNING",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        assert_eq!(body, "[2,1]"); // SUCCEEDED, RUNNING
    }

    #[tokio::test]
    async fn accept_spaces_around_commas() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=SUCCEEDED%2C+FAILED",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        assert_eq!(body, "[2,3]"); // SUCCEEDED, FAILED
    }

    #[tokio::test]
    async fn accept_trailing_comma() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=SUCCEEDED,",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        assert_eq!(body, "[2]"); // trailing comma ignored
    }

    #[tokio::test]
    async fn accept_single_job_status() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=KILLED",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        assert_eq!(body, "[4]");
    }

    #[tokio::test]
    async fn reject_missing_time_range_begin_millisecs() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/usage/compression?time_range_end_millisecs=100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(
            body.contains("time_range_begin_millisecs") || body.contains("deserialize"),
            "expected error about missing time_range_begin_millisecs, got: {body}"
        );
    }

    #[tokio::test]
    async fn reject_empty_job_status() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(
            body.contains("at least one valid status"),
            "expected error about empty job_status, got: {body}"
        );
    }

    #[tokio::test]
    async fn reject_zero_limit() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&limit=0",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(
            body.contains("limit must be > 0"),
            "expected error about limit, got: {body}"
        );
    }

    #[tokio::test]
    async fn reject_negative_limit() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&limit=-1",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(
            body.contains("limit must be > 0"),
            "expected error about limit, got: {body}"
        );
    }

    #[tokio::test]
    async fn reject_duplicate_job_status() {
        let app = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/usage/compression?time_range_begin_millisecs=0&\
                         time_range_end_millisecs=100&job_status=SUCCEEDED,SUCCEEDED",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(
            body.contains("Duplicate job_status"),
            "expected error about duplicate job_status, got: {body}"
        );
    }
}

#[cfg(test)]
mod stream_query_tests {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::net::SocketAddr;
    use std::ops::Range;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::MutexGuard;
    use std::time::Duration;
    use std::time::Instant;

    use async_trait::async_trait;
    use axum::Json;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::Response;
    use axum::routing::post;
    use clp_rust_utils::job_config::NetworkOutput;
    use clp_rust_utils::job_config::QueryJobId;
    use clp_rust_utils::job_config::QueryJobStatus;
    use clp_rust_utils::job_config::SearchJobConfig;
    use http_body_util::BodyExt;
    use search_result_listener::JobStatusSource;
    use search_result_listener::ListenerConfig;
    use search_result_listener::ResultListener;
    use serde_json::json;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::time::sleep;
    use tokio::time::timeout;
    use tower::ServiceExt;

    use super::serve_streaming_search;
    use crate::client::ClientError;
    use crate::client::QueryConfig;
    use crate::streaming_search::QueryJobTable;
    use crate::streaming_search::StreamingSearch;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// The longest a test waits for the server before failing.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// A [`QueryJobTable`] that keeps its query jobs in memory, and whose job statuses the test
    /// sets.
    #[derive(Clone, Default)]
    struct FakeQueryJobTable {
        state: Arc<Mutex<FakeQueryJobTableState>>,
    }

    #[derive(Default)]
    struct FakeQueryJobTableState {
        jobs: HashMap<QueryJobId, FakeQueryJob>,
        cancel_requests: Vec<QueryJobId>,
    }

    struct FakeQueryJob {
        search_job_config: SearchJobConfig,
        status: QueryJobStatus,
    }

    impl FakeQueryJobTable {
        /// # Returns
        ///
        /// The number of submitted query jobs.
        fn num_jobs(&self) -> usize {
            self.lock().jobs.len()
        }

        /// # Returns
        ///
        /// The IDs of the query jobs whose cancellation was requested, in request order.
        fn cancel_requests(&self) -> Vec<QueryJobId> {
            self.lock().cancel_requests.clone()
        }

        /// # Returns
        ///
        /// The config the query job `query_job_id` was submitted with.
        ///
        /// # Panics
        ///
        /// Panics if the query job doesn't exist.
        fn search_job_config(&self, query_job_id: QueryJobId) -> SearchJobConfig {
            self.lock()
                .jobs
                .get(&query_job_id)
                .expect("the query job should exist")
                .search_job_config
                .clone()
        }

        /// # Returns
        ///
        /// The status of the query job `query_job_id`.
        ///
        /// # Panics
        ///
        /// Panics if the query job doesn't exist.
        fn status(&self, query_job_id: QueryJobId) -> QueryJobStatus {
            self.lock()
                .jobs
                .get(&query_job_id)
                .expect("the query job should exist")
                .status
        }

        /// Sets the status of the query job `query_job_id`.
        ///
        /// # Panics
        ///
        /// Panics if the query job doesn't exist.
        fn set_status(&self, query_job_id: QueryJobId, status: QueryJobStatus) {
            self.lock()
                .jobs
                .get_mut(&query_job_id)
                .expect("the query job should exist")
                .status = status;
        }

        /// Waits until the cancellation of at least one query job has been requested.
        ///
        /// # Returns
        ///
        /// The IDs of the query jobs whose cancellation was requested, in request order.
        async fn wait_for_cancel_requests(&self) -> Vec<QueryJobId> {
            timeout(TIMEOUT, async {
                loop {
                    let cancel_requests = self.cancel_requests();
                    if !cancel_requests.is_empty() {
                        return cancel_requests;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the query job's cancellation should be requested")
        }

        /// # Panics
        ///
        /// Panics if the state lock is poisoned.
        fn lock(&self) -> MutexGuard<'_, FakeQueryJobTableState> {
            self.state
                .lock()
                .expect("the state lock shouldn't be poisoned")
        }
    }

    #[async_trait]
    impl JobStatusSource for FakeQueryJobTable {
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(10)
        }

        async fn get_status(
            &self,
            query_job_id: QueryJobId,
        ) -> Result<QueryJobStatus, search_result_listener::Error> {
            self.lock()
                .jobs
                .get(&query_job_id)
                .map(|job| job.status)
                .ok_or(search_result_listener::Error::QueryJobNotFound(
                    query_job_id,
                ))
        }
    }

    #[async_trait]
    impl QueryJobTable for FakeQueryJobTable {
        type StatusSource = Self;

        async fn submit(
            &self,
            search_job_config: &SearchJobConfig,
        ) -> Result<QueryJobId, ClientError> {
            let mut state = self.lock();
            let query_job_id = QueryJobId::try_from(state.jobs.len() + 1)
                .expect("the number of test query jobs should fit in `QueryJobId`");
            state.jobs.insert(
                query_job_id,
                FakeQueryJob {
                    search_job_config: search_job_config.clone(),
                    status: QueryJobStatus::Pending,
                },
            );
            drop(state);
            Ok(query_job_id)
        }

        async fn cancel(&self, query_job_id: QueryJobId) -> Result<bool, ClientError> {
            let mut state = self.lock();
            state.cancel_requests.push(query_job_id);
            let is_marked = state.jobs.get_mut(&query_job_id).is_some_and(|job| {
                let is_cancellable = matches!(
                    job.status,
                    QueryJobStatus::Pending | QueryJobStatus::Running
                );
                if is_cancellable {
                    job.status = QueryJobStatus::Cancelling;
                }
                is_cancellable
            });
            drop(state);
            Ok(is_marked)
        }

        fn status_source(&self) -> Self {
            self.clone()
        }
    }

    /// A streaming search app on a loopback result listener, whose query jobs the test controls.
    struct Harness {
        app: axum::Router,
        query_job_table: FakeQueryJobTable,
    }

    impl Harness {
        async fn start() -> Self {
            let listener = ResultListener::bind(ListenerConfig {
                bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                ..ListenerConfig::default()
            })
            .await
            .expect("binding a loopback listener should succeed");
            let query_job_table = FakeQueryJobTable::default();
            let streaming_search = StreamingSearch::new(listener, query_job_table.clone());
            Self {
                app: streaming_search_app(Some(Arc::new(streaming_search))),
                query_job_table,
            }
        }

        /// Submits a streaming search whose config is `query_config`.
        ///
        /// # Returns
        ///
        /// The server's response.
        async fn post(&self, query_config: &serde_json::Value) -> Response {
            post_query_stream(&self.app, query_config).await
        }

        /// Submits a streaming search for `*`, and reads its `job` event.
        ///
        /// # Returns
        ///
        /// A reader of the response's remaining events, and the search's query job ID.
        async fn start_search(&self) -> (SseReader, QueryJobId) {
            let response = self.post(&json!({"query_string": "*"})).await;
            assert_eq!(response.status(), StatusCode::OK);
            let mut events = SseReader::new(response);
            let job_event = events
                .next_event()
                .await
                .expect("a `job` event should arrive");
            assert_eq!(job_event.name.as_deref(), Some("job"));
            let query_job_id = job_event.data["query_job_id"]
                .as_i64()
                .and_then(|id| QueryJobId::try_from(id).ok())
                .expect("the `job` event should carry the query job ID");
            (events, query_job_id)
        }

        /// Connects a simulated search task to the session of the query job `query_job_id`.
        async fn connect(&self, query_job_id: QueryJobId) -> FakeClpS {
            let network_output = self
                .query_job_table
                .search_job_config(query_job_id)
                .network_output
                .expect("the query job should have a network output");
            FakeClpS::connect(&network_output).await
        }
    }

    /// An event read from a Server-Sent Events stream.
    #[derive(Debug)]
    struct SseEvent {
        name: Option<String>,
        data: serde_json::Value,
    }

    /// Reads the events of a Server-Sent Events response, skipping keep-alive comments.
    struct SseReader {
        body: Body,
        buffer: String,
    }

    impl SseReader {
        fn new(response: Response) -> Self {
            Self {
                body: response.into_body(),
                buffer: String::new(),
            }
        }

        /// # Returns
        ///
        /// The next event, or `None` once the response has ended.
        async fn next_event(&mut self) -> Option<SseEvent> {
            loop {
                if let Some(event_len) = self.buffer.find("\n\n") {
                    let block: String = self.buffer.drain(..event_len + 2).collect();
                    let mut name = None;
                    let mut data = None;
                    for line in block.lines() {
                        if let Some(value) = line.strip_prefix("event: ") {
                            name = Some(value.to_owned());
                        } else if let Some(value) = line.strip_prefix("data: ") {
                            data = Some(
                                serde_json::from_str(value).expect("event data should be JSON"),
                            );
                        }
                    }
                    if let Some(data) = data {
                        return Some(SseEvent { name, data });
                    }
                    continue;
                }
                let Some(frame) = timeout(TIMEOUT, self.body.frame())
                    .await
                    .expect("the next frame should arrive")
                else {
                    assert_eq!(self.buffer, "", "the response shouldn't end mid-event");
                    return None;
                };
                if let Ok(bytes) = frame.expect("the response should be intact").into_data() {
                    self.buffer
                        .push_str(std::str::from_utf8(&bytes).expect("events should be UTF-8"));
                }
            }
        }
    }

    /// A simulated `clp-s` search task that streams the results of task 0.
    struct FakeClpS {
        stream: TcpStream,
    }

    impl FakeClpS {
        /// Connects to the listener named by `network_output` and sends the handshake.
        async fn connect(network_output: &NetworkOutput) -> Self {
            let mut stream =
                TcpStream::connect((network_output.host.as_str(), network_output.port.get()))
                    .await
                    .expect("connecting to the listener should succeed");
            let handshake = rmp_serde::to_vec(&(
                1_u8,
                network_output.session_token.to_string(),
                0_u64,
                ARCHIVE_ID,
            ))
            .expect("encoding the handshake shouldn't fail");
            stream
                .write_all(&handshake)
                .await
                .expect("sending the handshake should succeed");
            Self { stream }
        }

        /// Sends the results at `result_indices`.
        async fn send_results(&mut self, result_indices: Range<u64>) {
            let mut bytes = Vec::new();
            for result_index in result_indices {
                bytes.extend(
                    rmp_serde::to_vec(&(
                        result_index,
                        timestamp(result_index),
                        format!("{}\n", message(result_index)),
                    ))
                    .expect("encoding a result shouldn't fail"),
                );
            }
            self.stream
                .write_all(&bytes)
                .await
                .expect("sending results should succeed");
        }
    }

    /// Builds an app that serves streaming searches through `streaming_search`, the way the API
    /// server's route does.
    fn streaming_search_app(
        streaming_search: Option<Arc<StreamingSearch<FakeQueryJobTable>>>,
    ) -> axum::Router {
        axum::Router::new().route(
            "/query/stream",
            post(move |Json(query_config): Json<QueryConfig>| {
                let streaming_search = streaming_search.clone();
                async move {
                    serve_streaming_search(streaming_search.as_deref(), query_config).await
                }
            }),
        )
    }

    /// Posts `query_config` to `app`'s streaming search route.
    ///
    /// # Returns
    ///
    /// The app's response.
    async fn post_query_stream(app: &axum::Router, query_config: &serde_json::Value) -> Response {
        app.clone()
            .oneshot(
                Request::post("/query/stream")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(query_config.to_string()))
                    .expect("the request should be valid"),
            )
            .await
            .expect("the app should respond")
    }

    /// # Returns
    ///
    /// The body of `response`.
    async fn body_text(response: Response) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("the body should be readable")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("the body should be UTF-8")
    }

    /// # Returns
    ///
    /// The timestamp of the result at `result_index`.
    fn timestamp(result_index: u64) -> i64 {
        1_700_000_000_000 + i64::try_from(result_index).expect("test result indices fit in `i64`")
    }

    /// # Returns
    ///
    /// The message of the result at `result_index`, without a trailing newline.
    fn message(result_index: u64) -> String {
        format!("{{\"result\":{result_index}}}")
    }

    #[tokio::test]
    async fn unsupported_or_invalid_query_configs_are_rejected_before_submission() {
        let harness = Harness::start().await;
        let rejected_query_configs = [
            (
                json!({"query_string": "*", "count_by_time_bucket_size_millisecs": 1000}),
                "count_by_time_bucket_size_millisecs",
            ),
            (
                json!({"query_string": "*", "buffer_results_in_mongodb": true}),
                "buffer_results_in_mongodb",
            ),
            (
                json!({"query_string": "*", "max_num_results": 10}),
                "max_num_results",
            ),
            (json!({"query_string": ""}), "query_string"),
            (
                json!({
                    "query_string": "*",
                    "time_range_begin_millisecs": 2,
                    "time_range_end_millisecs": 1
                }),
                "time_range_begin_millisecs",
            ),
        ];

        for (query_config, rejected_field) in rejected_query_configs {
            let response = harness.post(&query_config).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "unexpected status for {query_config}"
            );
            let body = body_text(response).await;
            assert!(
                body.contains(rejected_field),
                "the error should name `{rejected_field}`: {body}"
            );
        }
        assert_eq!(harness.query_job_table.num_jobs(), 0);
    }

    #[tokio::test]
    async fn streaming_search_without_spider_is_not_implemented() {
        let app = streaming_search_app(None);

        let response = post_query_stream(&app, &json!({"query_string": "*"})).await;

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = body_text(response).await;
        assert!(
            body.contains("Spider"),
            "the error should mention Spider: {body}"
        );
    }

    #[tokio::test]
    async fn search_streams_its_job_then_its_results_then_its_end() {
        const NUM_RESULTS: u64 = 3;

        let harness = Harness::start().await;
        let response = harness
            .post(&json!({
                "query_string": "*Transmitted*",
                "time_range_begin_millisecs": 1,
                "time_range_end_millisecs": 2,
                "ignore_case": true
            }))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .map(axum::http::HeaderValue::as_bytes),
            Some(b"text/event-stream".as_slice())
        );
        let mut events = SseReader::new(response);
        let job_event = events
            .next_event()
            .await
            .expect("a `job` event should arrive");
        assert_eq!(job_event.name.as_deref(), Some("job"));
        assert_eq!(job_event.data, json!({"query_job_id": 1}));

        let search_job_config = harness.query_job_table.search_job_config(1);
        let network_output = search_job_config
            .network_output
            .clone()
            .expect("the query job should have a network output");
        assert_eq!(
            search_job_config,
            SearchJobConfig {
                datasets: Some(vec!["default".to_owned()]),
                query_string: "*Transmitted*".to_owned(),
                begin_timestamp: Some(1),
                end_timestamp: Some(2),
                ignore_case: true,
                network_output: Some(network_output.clone()),
                ..SearchJobConfig::default()
            }
        );

        let mut clp_s = FakeClpS::connect(&network_output).await;
        clp_s.send_results(0..NUM_RESULTS).await;
        drop(clp_s);
        let mut results = Vec::new();
        for _ in 0..NUM_RESULTS {
            let result_event = events.next_event().await.expect("a result should arrive");
            assert_eq!(result_event.name, None);
            results.push(result_event.data);
        }
        harness
            .query_job_table
            .set_status(1, QueryJobStatus::Succeeded);
        let end_event = events
            .next_event()
            .await
            .expect("an `end` event should arrive");

        assert_eq!(
            results,
            (0..NUM_RESULTS)
                .map(|result_index| json!({
                    "archive_id": ARCHIVE_ID,
                    "timestamp": timestamp(result_index),
                    "message": message(result_index),
                }))
                .collect::<Vec<_>>()
        );
        assert_eq!(end_event.name.as_deref(), Some("end"));
        assert_eq!(
            end_event.data,
            json!({
                "status": "Succeeded",
                "num_results_emitted": NUM_RESULTS,
                "num_duplicates_dropped": 0,
                "num_protocol_errors": 0
            })
        );
        assert!(
            events.next_event().await.is_none(),
            "the response should end after the `end` event"
        );
    }

    #[tokio::test]
    async fn dropping_the_response_cancels_the_running_job_once() {
        let harness = Harness::start().await;
        let (events, query_job_id) = harness.start_search().await;
        harness
            .query_job_table
            .set_status(query_job_id, QueryJobStatus::Running);

        drop(events);

        assert_eq!(
            harness.query_job_table.wait_for_cancel_requests().await,
            [query_job_id]
        );
        assert_eq!(
            harness.query_job_table.status(query_job_id),
            QueryJobStatus::Cancelling
        );
        sleep(Duration::from_millis(200)).await;
        assert_eq!(harness.query_job_table.cancel_requests(), [query_job_id]);
    }

    #[tokio::test]
    async fn task_keeps_streaming_to_the_session_after_the_response_is_dropped() {
        let harness = Harness::start().await;
        let (events, query_job_id) = harness.start_search().await;
        let mut clp_s = harness.connect(query_job_id).await;
        clp_s.send_results(0..1).await;
        drop(events);
        harness.query_job_table.wait_for_cancel_requests().await;

        clp_s.send_results(1..100).await;
        let mut byte = [0_u8; 1];
        let read_result = timeout(Duration::from_millis(200), clp_s.stream.read(&mut byte)).await;

        assert!(
            read_result.is_err(),
            "the listener shouldn't close the task's connection: {read_result:?}"
        );
    }

    #[tokio::test]
    async fn dropping_the_response_after_its_end_never_cancels_the_job() {
        let harness = Harness::start().await;
        let (mut events, query_job_id) = harness.start_search().await;
        harness
            .query_job_table
            .set_status(query_job_id, QueryJobStatus::Succeeded);
        let end_event = events
            .next_event()
            .await
            .expect("an `end` event should arrive");
        assert_eq!(end_event.name.as_deref(), Some("end"));

        drop(events);
        sleep(Duration::from_millis(200)).await;

        assert_eq!(
            harness.query_job_table.cancel_requests(),
            [] as [QueryJobId; 0]
        );
        assert_eq!(
            harness.query_job_table.status(query_job_id),
            QueryJobStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn dropping_the_response_of_a_terminated_job_keeps_its_terminal_status() {
        let harness = Harness::start().await;
        let (mut events, query_job_id) = harness.start_search().await;
        // The open connection keeps the session draining after the job has terminated.
        let mut clp_s = harness.connect(query_job_id).await;
        clp_s.send_results(0..1).await;
        events.next_event().await.expect("a result should arrive");
        harness
            .query_job_table
            .set_status(query_job_id, QueryJobStatus::Succeeded);
        sleep(Duration::from_millis(100)).await;

        drop(events);

        assert_eq!(
            harness.query_job_table.wait_for_cancel_requests().await,
            [query_job_id]
        );
        assert_eq!(
            harness.query_job_table.status(query_job_id),
            QueryJobStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn closed_client_connection_is_detected_without_results_flowing() {
        /// Well below the SSE keep-alive interval, so that detecting the close can't rely on a
        /// keep-alive write failing.
        const MAX_DETECTION_LATENCY: Duration = Duration::from_secs(2);

        let harness = Harness::start().await;
        let tcp_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("binding a loopback listener should succeed");
        let server_addr = tcp_listener
            .local_addr()
            .expect("the bound listener should have an address");
        let app = harness.app.clone();
        let server = tokio::spawn(async move { axum::serve(tcp_listener, app).await });
        let mut client = TcpStream::connect(server_addr)
            .await
            .expect("connecting to the server should succeed");
        let body = json!({"query_string": "*"}).to_string();
        client
            .write_all(
                format!(
                    "POST /query/stream HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
                     application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("sending the request should succeed");
        let mut response = Vec::new();
        while !String::from_utf8_lossy(&response).contains("event: job") {
            let mut buffer = [0_u8; 1024];
            let num_bytes_read = timeout(TIMEOUT, client.read(&mut buffer))
                .await
                .expect("the `job` event should arrive")
                .expect("reading the response should succeed");
            assert_ne!(
                num_bytes_read, 0,
                "the server shouldn't close the connection"
            );
            response.extend_from_slice(&buffer[..num_bytes_read]);
        }

        let closed_at = Instant::now();
        drop(client);
        let cancel_requests = harness.query_job_table.wait_for_cancel_requests().await;
        let detection_latency = closed_at.elapsed();
        server.abort();

        assert_eq!(cancel_requests, [1]);
        assert!(
            detection_latency < MAX_DETECTION_LATENCY,
            "detecting the closed connection took {detection_latency:?}"
        );
    }
}
