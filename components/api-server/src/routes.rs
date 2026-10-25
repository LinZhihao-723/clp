use axum::Json;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::Sse;
use axum::response::sse::Event;
use axum::response::sse::KeepAlive;
use axum::routing::get;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::types::ArchiveId;
use futures::Stream;
use futures::StreamExt;
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

use crate::arrow_ipc;
use crate::arrow_ipc::ArrowStreamResponse;
use crate::client::Client;
use crate::client::ClientError;
use crate::client::CompressionUsage;
use crate::client::CompressionUsageParams;
use crate::client::QueryConfig;
use crate::client::ValidatedCompressionUsageParams;
use crate::negotiation::NegotiatedEncoding;
use crate::sse;
use crate::sse::SseResponse;
use crate::streaming_search::QueryJobTable;
use crate::streaming_search::SearchEvent;
use crate::streaming_search::SearchStream;
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
    description = "Submits a new search job and streams its results back in the same response: \
        as an Arrow IPC stream if the `Accept` header accepts \
        `application/vnd.apache.arrow.stream` with a nonzero quality that is at least the quality \
        of `text/event-stream`, or as Server-Sent Events (SSE) otherwise. Only \
        available when the package runs queries on Spider. If the client disconnects before the \
        end of the response, or the job's status can no longer be tracked, the job is marked as \
        cancelling.",
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
            description = "The search's stream, in the negotiated content type.\n\n\
                `text/event-stream`: first a `job` event whose data is a `StreamingSearchJob`, \
                then one unnamed event per result whose data is a `StreamingSearchResult`, and \
                finally an `end` event whose data is a `StreamingSearchEnd`, sent once the job has \
                terminated. If the job's status can't be tracked, an `error` event whose data is a \
                `StreamingSearchError` replaces the `end` event.\n\n\
                `application/vnd.apache.arrow.stream`: an Arrow IPC stream of the results with \
                the schema `{archive_id: Dictionary<Int32, Utf8>, timestamp: Int64 (epoch \
                milliseconds), message: Utf8 (without its trailing newline)}`, in which each \
                record batch follows its own replacement dictionary of archive IDs. Once the job \
                has terminated, the stream ends, and a second IPC stream of one row follows: \
                `{status: Utf8, num_results_emitted: UInt64, num_duplicates_dropped: UInt64, \
                num_protocol_errors: UInt64, num_incomplete_connections: UInt64}`, with the \
                fields of `StreamingSearchEnd`, all null if the job's status can't be tracked.",
            headers(
                (
                    "X-Clp-Query-Job-Id" = i32,
                    description = "The ID of the search's query job. Only sent with an Arrow IPC \
                        stream."
                )
            ),
            content(
                (
                    String = "text/event-stream",
                    example = "event: job\ndata: {\"query_job_id\":1}\n\ndata: \
                        {\"archive_id\":\"018e90e5-8b2a-4a61-a2fc-cac799936caf\",\"timestamp\":\
                        1633036800000,\"message\":\"Example log message\"}\n\nevent: end\ndata: \
                        {\"status\":\"Succeeded\",\"num_results_emitted\":1,\
                        \"num_duplicates_dropped\":0,\"num_protocol_errors\":0,\
                        \"num_incomplete_connections\":0}\n\n"
                ),
                (arrow_ipc::ArrowIpcStream = "application/vnd.apache.arrow.stream")
            )
        ),
        (
            status = BAD_REQUEST,
            description = "The query config is invalid (e.g., an empty `query_string`, an empty \
                `datasets`, or an invalid dataset name), or sets an option that streaming search \
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
    headers: HeaderMap,
    Json(query_config): Json<QueryConfig>,
) -> Result<Response, HandlerError> {
    serve_streaming_search(
        client.streaming_search(),
        query_config,
        NegotiatedEncoding::from_headers(&headers),
    )
    .await
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

    /// The number of connections from search tasks that ended before their results were
    /// acknowledged, without violating the wire protocol.
    num_incomplete_connections: u64,
}

impl From<SessionOutcome> for StreamingSearchEnd {
    fn from(outcome: SessionOutcome) -> Self {
        Self {
            status: outcome.status,
            num_results_emitted: outcome.stats.num_results_emitted,
            num_duplicates_dropped: outcome.stats.num_duplicates_dropped,
            num_protocol_errors: outcome.stats.num_protocol_errors,
            num_incomplete_connections: outcome.stats.num_incomplete_connections,
        }
    }
}

/// The data of a streaming search's `error` event.
#[derive(Serialize, ToSchema)]
struct StreamingSearchError {
    /// A description of the error.
    message: String,
}

/// Submits a streaming search, and streams its events back with `encoding`.
///
/// # Type Parameters
///
/// * `QueryJobTableType` - The query jobs table that the search is submitted to.
///
/// # Returns
///
/// The response on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`HandlerError::NotImplemented`] if `streaming_search` is `None`, i.e., the package doesn't
///   run queries on Spider.
/// * Forwards [`StreamingSearch::submit`]'s return values on failure.
/// * Forwards [`sse_response`]'s return values on failure.
async fn serve_streaming_search<QueryJobTableType: QueryJobTable>(
    streaming_search: Option<&StreamingSearch<QueryJobTableType, NegotiatedEncoding>>,
    query_config: QueryConfig,
    encoding: NegotiatedEncoding,
) -> Result<Response, HandlerError> {
    tracing::debug!(
        query_config = ? query_config,
        encoding = ? encoding,
        "Received a streaming search request."
    );
    let Some(streaming_search) = streaming_search else {
        return Err(HandlerError::NotImplemented(
            "streaming search requires the package to run queries on Spider".to_owned(),
        ));
    };
    let search_stream = streaming_search
        .submit(query_config, encoding)
        .await
        .inspect_err(|e| tracing::error!(error = % e, "Failed to submit a streaming search."))?;
    match encoding {
        NegotiatedEncoding::Sse => Ok(sse_response(search_stream)?.into_response()),
        NegotiatedEncoding::ArrowIpc => Ok(arrow_response(search_stream).into_response()),
    }
}

/// Streams the events of a submitted streaming search back as Server-Sent Events.
///
/// # Type Parameters
///
/// * `QueryJobTableType` - The query jobs table that the search was submitted to.
///
/// # Returns
///
/// The SSE response on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`sse::encode_event`]'s return values on failure.
fn sse_response<QueryJobTableType: QueryJobTable>(
    search_stream: SearchStream<QueryJobTableType, Vec<Bytes>>,
) -> Result<
    SseResponse<impl Stream<Item = Result<Bytes, HandlerError>> + use<QueryJobTableType>>,
    HandlerError,
> {
    const END_EVENT: &str = "end";
    const ERROR_EVENT: &str = "error";
    const JOB_EVENT: &str = "job";

    let query_job_id = search_stream.query_job_id();
    let job_event = sse::encode_event(JOB_EVENT, &StreamingSearchJob { query_job_id })?;
    let mut has_streamed_result = false;
    let events = search_stream.flat_map(move |search_event| {
        let chunks: Vec<Result<Bytes, HandlerError>> = match search_event {
            SearchEvent::Results(batch) => {
                if !has_streamed_result {
                    has_streamed_result = true;
                    tracing::debug!(
                        query_job_id,
                        "Yielding the streaming search's first result."
                    );
                }
                batch.into_iter().map(Ok).collect()
            }
            SearchEvent::End(Ok(outcome)) => vec![
                sse::encode_event(END_EVENT, &StreamingSearchEnd::from(outcome))
                    .map_err(HandlerError::from),
            ],
            SearchEvent::End(Err(e)) => {
                tracing::error!(
                    query_job_id,
                    error = % e,
                    "Failed to wait for the streaming search's query job to terminate."
                );
                vec![
                    sse::encode_event(
                        ERROR_EVENT,
                        &StreamingSearchError {
                            message: "failed to wait for the query job to terminate".to_owned(),
                        },
                    )
                    .map_err(HandlerError::from),
                ]
            }
        };
        futures::stream::iter(chunks)
    });
    Ok(SseResponse::new(
        futures::stream::once(std::future::ready(Ok(job_event))).chain(events),
    ))
}

/// Streams the events of a submitted streaming search back as an Arrow IPC stream, followed by
/// the summary stream.
///
/// # Type Parameters
///
/// * `QueryJobTableType` - The query jobs table that the search was submitted to.
///
/// # Returns
///
/// The Arrow IPC stream response.
fn arrow_response<QueryJobTableType: QueryJobTable>(
    search_stream: SearchStream<QueryJobTableType, Vec<Bytes>>,
) -> ArrowStreamResponse<
    impl Stream<Item = Result<Bytes, std::convert::Infallible>> + use<QueryJobTableType>,
> {
    let query_job_id = search_stream.query_job_id();
    let mut has_streamed_result = false;
    let chunks = search_stream.flat_map(move |search_event| {
        let chunks: Vec<Bytes> = match search_event {
            SearchEvent::Results(batch) => {
                if !has_streamed_result {
                    has_streamed_result = true;
                    tracing::debug!(
                        query_job_id,
                        "Yielding the streaming search's first result."
                    );
                }
                batch
            }
            SearchEvent::End(outcome) => {
                if let Err(e) = &outcome {
                    tracing::error!(
                        query_job_id,
                        error = % e,
                        "Failed to wait for the streaming search's query job to terminate."
                    );
                }
                vec![
                    Bytes::from_static(arrow_ipc::END_OF_STREAM),
                    arrow_ipc::summary_stream(outcome.as_ref().ok()),
                ]
            }
        };
        futures::stream::iter(chunks.into_iter().map(Ok))
    });
    ArrowStreamResponse::new(
        query_job_id,
        futures::stream::once(std::future::ready(Ok(arrow_ipc::schema_message()))).chain(chunks),
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
    #[error("internal server error")]
    InternalServer,
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("not implemented: {0}")]
    NotImplemented(String),
}

impl From<serde_json::Error> for HandlerError {
    fn from(_: serde_json::Error) -> Self {
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
    use std::collections::BTreeMap;
    use std::convert::Infallible;
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
    use axum::body::Bytes;
    use axum::http::HeaderMap;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::http::header::ACCEPT;
    use axum::http::header::CACHE_CONTROL;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::response::Sse;
    use axum::response::sse::Event;
    use axum::routing::post;
    use clp_rust_utils::job_config::NetworkOutput;
    use clp_rust_utils::job_config::QueryJobId;
    use clp_rust_utils::job_config::QueryJobStatus;
    use clp_rust_utils::job_config::SearchJobConfig;
    use clp_rust_utils::types::ArchiveId;
    use futures::stream;
    use http_body_util::BodyExt;
    use search_result_listener::JobStatusSource;
    use search_result_listener::ListenerConfig;
    use search_result_listener::ResultEncoder;
    use search_result_listener::ResultEncoding;
    use search_result_listener::ResultListener;
    use search_result_listener::SessionOutcome;
    use search_result_listener::SessionStats;
    use serde_json::json;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::sync::Semaphore;
    use tokio::time::sleep;
    use tokio::time::timeout;
    use tower::ServiceExt;

    use super::StreamingSearchEnd;
    use super::StreamingSearchError;
    use super::StreamingSearchJob;
    use super::StreamingSearchResult;
    use super::serve_streaming_search;
    use crate::arrow_ipc::ARROW_STREAM_CONTENT_TYPE;
    use crate::arrow_ipc::QUERY_JOB_ID_HEADER;
    use crate::client::ClientError;
    use crate::client::QueryConfig;
    use crate::negotiation::NegotiatedEncoding;
    use crate::sse;
    use crate::sse::SseEncoding;
    use crate::streaming_search::QueryJobTable;
    use crate::streaming_search::StreamingSearch;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// Messages whose JSON escaping or trailing newlines an event encoding could get wrong.
    const TRICKY_MESSAGES: &[&str] = &[
        "{\"quoted\":\"value\"}\n",
        "back\\slash\n",
        "nul \u{0} unit separator \u{1f} delete \u{7f}\n",
        "tab\t backspace\u{8} form feed\u{c}\n",
        "line separator \u{2028} paragraph separator \u{2029}\n",
        "non-ASCII é 日本 🦀\n",
        "x\n",
        "x\n\n",
        "x\r\n",
        "inner\nnewline",
        "",
        "\n",
    ];

    /// Timestamps at the edges of their range.
    const TRICKY_TIMESTAMPS: &[i64] = &[i64::MIN, -1, 0, i64::MAX];

    /// The longest a test waits for the server before failing.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// A [`QueryJobTable`] that keeps its query jobs in memory, and whose job statuses the test
    /// sets.
    #[derive(Clone, Default)]
    struct FakeQueryJobTable {
        state: Arc<Mutex<FakeQueryJobTableState>>,
    }

    impl FakeQueryJobTable {
        /// # Returns
        ///
        /// The configs of the submitted query jobs, ordered by query job ID.
        fn search_job_configs(&self) -> Vec<SearchJobConfig> {
            self.lock()
                .jobs
                .values()
                .map(|job| job.search_job_config.clone())
                .collect()
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

        /// Makes every later poll of a query job's status fail, leaving the job's status unchanged.
        fn fail_status_polls(&self) {
            self.lock().should_fail_status_polls = true;
        }

        /// Makes every later submission insert its query job, then wait for a permit of the
        /// returned semaphore before returning the job's ID.
        ///
        /// # Returns
        ///
        /// The semaphore that releases the held submissions, which has no permits.
        fn hold_submissions(&self) -> Arc<Semaphore> {
            let submission_gate = Arc::new(Semaphore::new(0));
            self.lock().submission_gate = Some(submission_gate.clone());
            submission_gate
        }

        /// Waits until at least one query job has been submitted.
        ///
        /// # Returns
        ///
        /// The configs of the submitted query jobs, ordered by query job ID.
        async fn wait_for_search_job_configs(&self) -> Vec<SearchJobConfig> {
            timeout(TIMEOUT, async {
                loop {
                    let search_job_configs = self.search_job_configs();
                    if !search_job_configs.is_empty() {
                        return search_job_configs;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("a query job should be submitted")
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

        /// # Returns
        ///
        /// The guard of the fake's state.
        ///
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
    impl QueryJobTable for FakeQueryJobTable {
        type StatusSource = Self;

        async fn submit(
            &self,
            search_job_config: &SearchJobConfig,
        ) -> Result<QueryJobId, ClientError> {
            let (query_job_id, submission_gate) = {
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
                (query_job_id, state.submission_gate.clone())
            };
            if let Some(submission_gate) = submission_gate {
                let _permit = submission_gate
                    .acquire()
                    .await
                    .expect("the submission gate shouldn't be closed");
            }
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

    #[async_trait]
    impl JobStatusSource for FakeQueryJobTable {
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(10)
        }

        async fn get_status(
            &self,
            query_job_id: QueryJobId,
        ) -> Result<QueryJobStatus, search_result_listener::Error> {
            let state = self.lock();
            if state.should_fail_status_polls {
                return Err(search_result_listener::Error::Io(std::io::Error::other(
                    "the status poll failed",
                )));
            }
            state.jobs.get(&query_job_id).map(|job| job.status).ok_or(
                search_result_listener::Error::QueryJobNotFound(query_job_id),
            )
        }
    }

    #[derive(Default)]
    struct FakeQueryJobTableState {
        jobs: BTreeMap<QueryJobId, FakeQueryJob>,
        cancel_requests: Vec<QueryJobId>,
        should_fail_status_polls: bool,
        submission_gate: Option<Arc<Semaphore>>,
    }

    struct FakeQueryJob {
        search_job_config: SearchJobConfig,
        status: QueryJobStatus,
    }

    /// A streaming search app on a loopback result listener, whose query jobs the test controls.
    struct Harness {
        app: axum::Router,
        query_job_table: FakeQueryJobTable,
    }

    impl Harness {
        /// Starts a streaming search app on a loopback result listener.
        ///
        /// # Returns
        ///
        /// The started harness.
        ///
        /// # Panics
        ///
        /// Panics if the loopback listener can't be bound.
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
        ///
        /// # Returns
        ///
        /// The simulated search task, connected to the session and past its handshake.
        ///
        /// # Panics
        ///
        /// Panics if the query job doesn't exist or has no network output.
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
    #[derive(Debug, PartialEq)]
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
        /// # Returns
        ///
        /// A reader of `response`'s events.
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
        ///
        /// # Returns
        ///
        /// The simulated search task, connected and past its handshake.
        ///
        /// # Panics
        ///
        /// Panics if connecting or sending the handshake fails.
        async fn connect(network_output: &NetworkOutput) -> Self {
            let mut stream =
                TcpStream::connect((network_output.host.as_str(), network_output.port.get()))
                    .await
                    .expect("connecting to the listener should succeed");
            let handshake = rmp_serde::to_vec(&(
                2_u8,
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

        /// Sends `results` as the task's first results, each given by its timestamp and message.
        async fn send_timestamped_messages(&mut self, results: &[(i64, &str)]) {
            let mut bytes = Vec::new();
            for (result_index, (timestamp, message)) in (0_u64..).zip(results) {
                bytes.extend(
                    rmp_serde::to_vec(&(result_index, timestamp, message))
                        .expect("encoding a result shouldn't fail"),
                );
            }
            self.stream
                .write_all(&bytes)
                .await
                .expect("sending results should succeed");
        }

        /// Ends the stream the way `clp-s` does: sends a trailer counting `num_results` results,
        /// then EOF, and reads until the listener closes the connection.
        ///
        /// # Returns
        ///
        /// The bytes the listener sent back.
        ///
        /// # Panics
        ///
        /// Panics if sending the trailer or EOF, or reading the reply, fails.
        async fn finish(mut self, num_results: u64) -> Vec<u8> {
            let trailer =
                rmp_serde::to_vec(&(num_results,)).expect("encoding the trailer shouldn't fail");
            self.stream
                .write_all(&trailer)
                .await
                .expect("sending the trailer should succeed");
            self.stream
                .shutdown()
                .await
                .expect("sending EOF should succeed");
            let mut reply = Vec::new();
            timeout(TIMEOUT, self.stream.read_to_end(&mut reply))
                .await
                .expect("the listener should close the connection")
                .expect("reading the reply should succeed");
            reply
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

    /// A simulated search task that streams the results of task 0 with protocol v3, aligning the
    /// buffers of its IPC messages to arrow-rs's default of 64 bytes, unlike the server's 8.
    struct FakeArrowTask {
        stream: TcpStream,
        encoder: arrow_ipc::writer::StreamEncoder,
        archive_ids: arrow_array::ArrayRef,
    }

    impl FakeArrowTask {
        /// Connects to the listener named by `network_output` and sends the v3 handshake.
        ///
        /// # Returns
        ///
        /// The simulated search task, connected and past its handshake.
        ///
        /// # Panics
        ///
        /// Panics if connecting or sending the handshake fails.
        async fn connect(network_output: &NetworkOutput) -> Self {
            let mut stream =
                TcpStream::connect((network_output.host.as_str(), network_output.port.get()))
                    .await
                    .expect("connecting to the listener should succeed");
            let handshake = rmp_serde::to_vec(&(
                3_u8,
                network_output.session_token.to_string(),
                0_u64,
                ARCHIVE_ID,
            ))
            .expect("encoding the handshake shouldn't fail");
            stream
                .write_all(&handshake)
                .await
                .expect("sending the handshake should succeed");
            Self {
                stream,
                encoder: arrow_ipc::writer::StreamEncoder::try_new(
                    &search_result_listener::RESULT_SCHEMA,
                )
                .expect("the result schema should be encodable"),
                archive_ids: Arc::new(arrow_array::StringArray::from(vec![ARCHIVE_ID])),
            }
        }

        /// Sends the results at `result_indices` as one record batch, preceded by the schema and
        /// the dictionary on the first call.
        ///
        /// # Returns
        ///
        /// The bytes sent.
        async fn send_batch(&mut self, result_indices: Range<u64>) -> Vec<u8> {
            let num_rows = usize::try_from(result_indices.end - result_indices.start)
                .expect("the number of test results should fit in `usize`");
            let archive_ids =
                arrow_array::DictionaryArray::<arrow_array::types::Int32Type>::try_new(
                    arrow_array::Int32Array::from(vec![0; num_rows]),
                    Arc::clone(&self.archive_ids),
                )
                .expect("the keys should index the dictionary");
            let batch = arrow_array::RecordBatch::try_new(
                Arc::clone(&search_result_listener::RESULT_SCHEMA),
                vec![
                    Arc::new(archive_ids),
                    Arc::new(arrow_array::Int64Array::from_iter_values(
                        result_indices.clone().map(timestamp),
                    )),
                    Arc::new(arrow_array::StringArray::from_iter_values(
                        result_indices.map(message),
                    )),
                ],
            )
            .expect("the batch should match the result schema");
            let bytes: Vec<u8> = self
                .encoder
                .encode(&batch)
                .expect("the batch should encode")
                .iter()
                .flat_map(|buffer| buffer.to_vec())
                .collect();
            self.stream
                .write_all(&bytes)
                .await
                .expect("sending a batch should succeed");
            bytes
        }

        /// Ends the stream the way the Rust search task does: sends the end-of-stream marker and
        /// a trailer counting `num_results` results, then EOF, and reads until the listener closes
        /// the connection.
        ///
        /// # Returns
        ///
        /// The bytes the listener sent back.
        async fn finish(mut self, num_results: u64) -> Vec<u8> {
            let mut bytes: Vec<u8> = self
                .encoder
                .finish()
                .expect("the stream should end")
                .iter()
                .flat_map(|buffer| buffer.to_vec())
                .collect();
            bytes.extend(
                rmp_serde::to_vec(&(num_results,)).expect("encoding the trailer shouldn't fail"),
            );
            self.stream
                .write_all(&bytes)
                .await
                .expect("sending the trailer should succeed");
            self.stream
                .shutdown()
                .await
                .expect("sending EOF should succeed");
            let mut reply = Vec::new();
            timeout(TIMEOUT, self.stream.read_to_end(&mut reply))
                .await
                .expect("the listener should close the connection")
                .expect("reading the reply should succeed");
            reply
        }
    }

    /// One result decoded from an Arrow IPC stream response: `(archive_id, timestamp, message)`.
    type ArrowRow = (String, i64, String);

    /// The summary row of an Arrow IPC stream response.
    #[derive(Debug, PartialEq, Eq)]
    struct ArrowSummary {
        status: Option<String>,
        num_results_emitted: Option<u64>,
        num_duplicates_dropped: Option<u64>,
        num_protocol_errors: Option<u64>,
        num_incomplete_connections: Option<u64>,
    }

    /// Decodes the body of an Arrow IPC stream response with arrow-rs's stream reader.
    ///
    /// # Returns
    ///
    /// The results of the result stream, and the row of the summary stream that follows it.
    ///
    /// # Panics
    ///
    /// Panics if the body isn't a result stream followed by a summary stream of one row.
    fn decode_arrow_body(body: &[u8]) -> (Vec<ArrowRow>, ArrowSummary) {
        use arrow_array::Array;

        let mut remaining = body;
        let mut rows = Vec::new();
        let reader = arrow_ipc::reader::StreamReader::try_new(&mut remaining, None)
            .expect("the body should start with the result schema");
        assert_eq!(reader.schema(), *search_result_listener::RESULT_SCHEMA);
        for batch in reader {
            let batch = batch.expect("every result batch should decode");
            let archive_ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<arrow_array::DictionaryArray<arrow_array::types::Int32Type>>()
                .expect("archive IDs should be a dictionary");
            let archive_id_values = archive_ids
                .values()
                .as_any()
                .downcast_ref::<arrow_array::StringArray>()
                .expect("archive ID values should be strings");
            let timestamps = batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow_array::Int64Array>()
                .expect("timestamps should be `Int64`");
            let messages = batch
                .column(2)
                .as_any()
                .downcast_ref::<arrow_array::StringArray>()
                .expect("messages should be strings");
            for row in 0..batch.num_rows() {
                let key = usize::try_from(archive_ids.keys().value(row))
                    .expect("keys should be non-negative");
                rows.push((
                    archive_id_values.value(key).to_owned(),
                    timestamps.value(row),
                    messages.value(row).to_owned(),
                ));
            }
        }
        let batches: Vec<_> = arrow_ipc::reader::StreamReader::try_new(&mut remaining, None)
            .expect("the summary stream should follow")
            .collect::<Result<_, _>>()
            .expect("the summary should decode");
        assert_eq!(remaining, &[] as &[u8], "nothing should follow the summary");
        let [summary] = batches.as_slice() else {
            panic!("the summary stream should hold one batch: {batches:?}");
        };
        assert_eq!(summary.num_rows(), 1);
        let status = summary
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .expect("the status should be a string");
        let count = |column: usize| {
            let counts = summary
                .column(column)
                .as_any()
                .downcast_ref::<arrow_array::UInt64Array>()
                .expect("the counts should be `UInt64`");
            (!counts.is_null(0)).then(|| counts.value(0))
        };
        (
            rows,
            ArrowSummary {
                status: (!status.is_null(0)).then(|| status.value(0).to_owned()),
                num_results_emitted: count(1),
                num_duplicates_dropped: count(2),
                num_protocol_errors: count(3),
                num_incomplete_connections: count(4),
            },
        )
    }

    /// Posts `query_config` to `app`'s streaming search route, accepting an Arrow IPC stream.
    ///
    /// # Returns
    ///
    /// The app's response.
    async fn post_arrow_query_stream(
        app: &axum::Router,
        query_config: &serde_json::Value,
    ) -> Response {
        app.clone()
            .oneshot(
                Request::post("/query/stream")
                    .header(CONTENT_TYPE, "application/json")
                    .header(ACCEPT, ARROW_STREAM_CONTENT_TYPE)
                    .body(Body::from(query_config.to_string()))
                    .expect("the request should be valid"),
            )
            .await
            .expect("the app should respond")
    }

    /// # Returns
    ///
    /// The body of `response`, once it ends.
    async fn body_bytes(response: Response) -> Bytes {
        timeout(TIMEOUT, response.into_body().collect())
            .await
            .expect("the body should end")
            .expect("the body should be readable")
            .to_bytes()
    }

    /// # Returns
    ///
    /// The rows of the results at `result_indices`, as an Arrow IPC stream response holds them.
    fn expected_rows(result_indices: Range<u64>) -> Vec<ArrowRow> {
        result_indices
            .map(|result_index| {
                (
                    ARCHIVE_ID.to_owned(),
                    timestamp(result_index),
                    message(result_index),
                )
            })
            .collect()
    }

    /// Builds an app that serves streaming searches through `streaming_search`, the way the API
    /// server's route does.
    ///
    /// # Returns
    ///
    /// A router serving `POST /query/stream`.
    fn streaming_search_app(
        streaming_search: Option<Arc<StreamingSearch<FakeQueryJobTable, NegotiatedEncoding>>>,
    ) -> axum::Router {
        axum::Router::new().route(
            "/query/stream",
            post(
                move |headers: HeaderMap, Json(query_config): Json<QueryConfig>| {
                    let streaming_search = streaming_search.clone();
                    async move {
                        serve_streaming_search(
                            streaming_search.as_deref(),
                            query_config,
                            NegotiatedEncoding::from_headers(&headers),
                        )
                        .await
                    }
                },
            ),
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

    /// # Returns
    ///
    /// The listener's acknowledgement of `num_results` results: the msgpack array `[num_results]`
    /// with the count encoded as a `uint 64`.
    fn acknowledgement(num_results: u64) -> Vec<u8> {
        let mut bytes = vec![0x91, 0xcf];
        bytes.extend_from_slice(&num_results.to_be_bytes());
        bytes
    }

    /// # Returns
    ///
    /// Each of [`TRICKY_MESSAGES`], paired with one of [`TRICKY_TIMESTAMPS`] in turn.
    fn tricky_results() -> Vec<(i64, &'static str)> {
        TRICKY_TIMESTAMPS
            .iter()
            .copied()
            .cycle()
            .zip(TRICKY_MESSAGES.iter().copied())
            .collect()
    }

    /// # Returns
    ///
    /// The event that the streaming search route encoded with [`Event::json_data`] for a result.
    ///
    /// # Panics
    ///
    /// Panics if serializing the result fails.
    fn axum_result_event(archive_id: ArchiveId, timestamp: i64, message: &str) -> Event {
        let mut message = message.to_owned();
        if message.ends_with('\n') {
            message.pop();
        }
        Event::default()
            .json_data(StreamingSearchResult {
                archive_id,
                timestamp,
                message,
            })
            .expect("serializing a result shouldn't fail")
    }

    /// # Returns
    ///
    /// The body of an axum [`Sse`] response streaming `events`.
    async fn axum_sse_body(events: Vec<Event>) -> String {
        body_text(
            Sse::new(stream::iter(events.into_iter().map(Ok::<_, Infallible>))).into_response(),
        )
        .await
    }

    /// # Returns
    ///
    /// `bytes` as a string.
    ///
    /// # Panics
    ///
    /// Panics if `bytes` isn't UTF-8.
    fn utf8(bytes: &Bytes) -> String {
        String::from_utf8(bytes.to_vec()).expect("the bytes should be UTF-8")
    }

    #[tokio::test]
    async fn sse_encoder_matches_axum_json_events() -> anyhow::Result<()> {
        let archive_id = ARCHIVE_ID.parse::<ArchiveId>()?;
        let mut encoder = SseEncoding.encoder(archive_id);
        let mut axum_events = Vec::new();
        for (timestamp, message) in tricky_results() {
            // The listener hands messages to the encoders without their trailing newline.
            encoder.encode(timestamp, message.strip_suffix('\n').unwrap_or(message));
            axum_events.push(axum_result_event(archive_id, timestamp, message));
        }

        assert_eq!(
            utf8(&encoder.take_batch()),
            axum_sse_body(axum_events).await
        );
        Ok(())
    }

    #[test]
    fn end_event_carries_the_session_statistics() -> anyhow::Result<()> {
        let end = StreamingSearchEnd::from(SessionOutcome {
            status: QueryJobStatus::Failed,
            stats: SessionStats {
                num_results_emitted: 3,
                num_duplicates_dropped: 2,
                num_protocol_errors: 1,
                num_incomplete_connections: 4,
                num_results_discarded: 5,
            },
        });

        assert_eq!(
            serde_json::to_value(&end)?,
            json!({
                "status": "Failed",
                "num_results_emitted": 3,
                "num_duplicates_dropped": 2,
                "num_protocol_errors": 1,
                "num_incomplete_connections": 4
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn named_events_match_axum() -> anyhow::Result<()> {
        let job = StreamingSearchJob { query_job_id: 7 };
        let end = StreamingSearchEnd::from(SessionOutcome {
            status: QueryJobStatus::Succeeded,
            stats: SessionStats {
                num_results_emitted: 3,
                num_duplicates_dropped: 2,
                num_protocol_errors: 1,
                num_incomplete_connections: 4,
                num_results_discarded: 0,
            },
        });
        let error = StreamingSearchError {
            message: "failed to wait for the \"query\" job\nto terminate".to_owned(),
        };

        assert_eq!(
            utf8(&sse::encode_event("job", &job)?),
            axum_sse_body(vec![Event::default().event("job").json_data(&job)?]).await
        );
        assert_eq!(
            utf8(&sse::encode_event("end", &end)?),
            axum_sse_body(vec![Event::default().event("end").json_data(&end)?]).await
        );
        assert_eq!(
            utf8(&sse::encode_event("error", &error)?),
            axum_sse_body(vec![Event::default().event("error").json_data(&error)?]).await
        );
        Ok(())
    }

    #[tokio::test]
    async fn response_body_is_byte_identical_to_the_axum_sse_rendering() -> anyhow::Result<()> {
        let harness = Harness::start().await;
        let response = harness.post(&json!({"query_string": "*"})).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .map(axum::http::HeaderValue::as_bytes),
            Some(b"text/event-stream".as_slice())
        );
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .map(axum::http::HeaderValue::as_bytes),
            Some(b"no-cache".as_slice())
        );

        let results = tricky_results();
        let mut clp_s = harness.connect(1).await;
        clp_s.send_timestamped_messages(&results).await;
        assert_eq!(
            clp_s.finish(results.len() as u64).await,
            acknowledgement(results.len() as u64)
        );
        harness
            .query_job_table
            .set_status(1, QueryJobStatus::Succeeded);
        let body = timeout(TIMEOUT, body_text(response)).await?;

        let archive_id = ARCHIVE_ID.parse::<ArchiveId>()?;
        let mut axum_events = vec![
            Event::default()
                .event("job")
                .json_data(StreamingSearchJob { query_job_id: 1 })?,
        ];
        axum_events.extend(
            results
                .iter()
                .map(|(timestamp, message)| axum_result_event(archive_id, *timestamp, message)),
        );
        axum_events.push(
            Event::default()
                .event("end")
                .json_data(StreamingSearchEnd::from(SessionOutcome {
                    status: QueryJobStatus::Succeeded,
                    stats: SessionStats {
                        num_results_emitted: u64::try_from(results.len())?,
                        ..SessionStats::default()
                    },
                }))?,
        );
        assert_eq!(body, axum_sse_body(axum_events).await);
        Ok(())
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
            (json!({"query_string": "*", "datasets": []}), "datasets"),
            (
                json!({"query_string": "*", "datasets": ["default", "my-logs"]}),
                "my-logs",
            ),
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
        assert_eq!(
            harness.query_job_table.search_job_configs(),
            [] as [SearchJobConfig; 0]
        );
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
        assert_eq!(
            clp_s.finish(NUM_RESULTS).await,
            acknowledgement(NUM_RESULTS)
        );
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
                "num_protocol_errors": 0,
                "num_incomplete_connections": 0
            })
        );
        assert_eq!(
            events.next_event().await,
            None,
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
            .set_status(query_job_id, QueryJobStatus::Failed);
        sleep(Duration::from_millis(100)).await;

        drop(events);

        assert_eq!(
            harness.query_job_table.wait_for_cancel_requests().await,
            [query_job_id]
        );
        assert_eq!(
            harness.query_job_table.status(query_job_id),
            QueryJobStatus::Failed
        );
    }

    #[tokio::test]
    async fn dropping_the_request_during_its_submission_cancels_the_submitted_job() {
        let harness = Harness::start().await;
        let submission_gate = harness.query_job_table.hold_submissions();
        let app = harness.app.clone();
        let query_config = json!({"query_string": "*"});
        let request = tokio::spawn(async move { post_query_stream(&app, &query_config).await });
        harness.query_job_table.wait_for_search_job_configs().await;

        request.abort();
        assert!(
            request
                .await
                .expect_err("the request should be pending on its submission")
                .is_cancelled()
        );
        submission_gate.add_permits(1);

        assert_eq!(
            harness.query_job_table.wait_for_cancel_requests().await,
            [1]
        );
        assert_eq!(
            harness.query_job_table.status(1),
            QueryJobStatus::Cancelling
        );
    }

    #[tokio::test]
    async fn failing_to_track_the_job_ends_the_response_with_an_error_and_cancels_the_job_once() {
        let harness = Harness::start().await;
        let (mut events, query_job_id) = harness.start_search().await;
        harness
            .query_job_table
            .set_status(query_job_id, QueryJobStatus::Running);

        harness.query_job_table.fail_status_polls();
        let error_event = events
            .next_event()
            .await
            .expect("an `error` event should arrive");

        assert_eq!(
            error_event,
            SseEvent {
                name: Some("error".to_owned()),
                data: json!({"message": "failed to wait for the query job to terminate"}),
            }
        );
        assert_eq!(
            harness.query_job_table.wait_for_cancel_requests().await,
            [query_job_id]
        );
        assert_eq!(
            harness.query_job_table.status(query_job_id),
            QueryJobStatus::Cancelling
        );
        assert_eq!(
            events.next_event().await,
            None,
            "the response should end after the `error` event"
        );
        drop(events);
        sleep(Duration::from_millis(200)).await;
        assert_eq!(harness.query_job_table.cancel_requests(), [query_job_id]);
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

    #[tokio::test]
    async fn arrow_response_streams_the_schema_then_the_forwarded_batches_then_the_summary() {
        let harness = Harness::start().await;
        let response = post_arrow_query_stream(&harness.app, &json!({"query_string": "*"})).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .map(axum::http::HeaderValue::as_bytes),
            Some(ARROW_STREAM_CONTENT_TYPE.as_bytes())
        );
        assert_eq!(
            response
                .headers()
                .get(QUERY_JOB_ID_HEADER)
                .map(axum::http::HeaderValue::as_bytes),
            Some(b"1".as_slice())
        );

        let network_output = harness
            .query_job_table
            .search_job_config(1)
            .network_output
            .expect("the query job should have a network output");
        let mut task = FakeArrowTask::connect(&network_output).await;
        let first = task.send_batch(0..3).await;
        let second = task.send_batch(3..1000).await;
        assert_eq!(task.finish(1000).await, acknowledgement(1000));
        harness
            .query_job_table
            .set_status(1, QueryJobStatus::Succeeded);
        let body = body_bytes(response).await;
        let (rows, summary) = decode_arrow_body(&body);

        assert_eq!(rows, expected_rows(0..1000));
        assert_eq!(
            summary,
            ArrowSummary {
                status: Some("Succeeded".to_owned()),
                num_results_emitted: Some(1000),
                num_duplicates_dropped: Some(0),
                num_protocol_errors: Some(0),
                num_incomplete_connections: Some(0),
            }
        );
        // Without the stream's schema message, the producer's bytes hold the dictionary and the
        // record batch, which the response forwards as they are.
        let schema_len = 8 + usize::try_from(u32::from_le_bytes(
            first[4..8]
                .try_into()
                .expect("the metadata length should be 4 bytes"),
        ))
        .expect("the metadata length should fit in `usize`");
        for forwarded in [&first[schema_len..], &second[..]] {
            assert!(
                body.windows(forwarded.len())
                    .any(|window| window == forwarded),
                "the response should forward the producer's messages byte for byte"
            );
        }
    }

    #[tokio::test]
    async fn arrow_response_reencodes_only_the_unclaimed_suffix_of_a_partial_duplicate() {
        let harness = Harness::start().await;
        let response = post_arrow_query_stream(&harness.app, &json!({"query_string": "*"})).await;
        let network_output = harness
            .query_job_table
            .search_job_config(1)
            .network_output
            .expect("the query job should have a network output");
        let mut failed_attempt = FakeArrowTask::connect(&network_output).await;
        failed_attempt.send_batch(0..4).await;
        sleep(Duration::from_millis(200)).await;
        drop(failed_attempt);

        let mut retry = FakeArrowTask::connect(&network_output).await;
        retry.send_batch(0..2).await;
        retry.send_batch(2..6).await;
        retry.send_batch(6..7).await;
        assert_eq!(retry.finish(7).await, acknowledgement(7));
        harness
            .query_job_table
            .set_status(1, QueryJobStatus::Succeeded);
        let (rows, summary) = decode_arrow_body(&body_bytes(response).await);

        assert_eq!(rows, expected_rows(0..7));
        assert_eq!(summary.num_results_emitted, Some(7));
        assert_eq!(summary.num_duplicates_dropped, Some(4));
        assert_eq!(summary.num_incomplete_connections, Some(1));
    }

    #[tokio::test]
    async fn arrow_response_encodes_protocol_v2_results_row_by_row() {
        let harness = Harness::start().await;
        let response = post_arrow_query_stream(&harness.app, &json!({"query_string": "*"})).await;
        let results = tricky_results();
        let mut clp_s = harness.connect(1).await;
        clp_s.send_timestamped_messages(&results).await;
        assert_eq!(
            clp_s.finish(results.len() as u64).await,
            acknowledgement(results.len() as u64)
        );
        harness
            .query_job_table
            .set_status(1, QueryJobStatus::Succeeded);
        let (rows, summary) = decode_arrow_body(&body_bytes(response).await);

        assert_eq!(
            rows,
            results
                .iter()
                .map(|(timestamp, message)| (
                    ARCHIVE_ID.to_owned(),
                    *timestamp,
                    message.strip_suffix('\n').unwrap_or(message).to_owned()
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(summary.num_results_emitted, Some(results.len() as u64));
    }

    #[tokio::test]
    async fn sse_response_encodes_protocol_v3_results_as_events() {
        const NUM_RESULTS: u64 = 5;

        let harness = Harness::start().await;
        let (mut events, query_job_id) = harness.start_search().await;
        let network_output = harness
            .query_job_table
            .search_job_config(query_job_id)
            .network_output
            .expect("the query job should have a network output");
        let mut task = FakeArrowTask::connect(&network_output).await;
        task.send_batch(0..2).await;
        task.send_batch(2..NUM_RESULTS).await;
        assert_eq!(task.finish(NUM_RESULTS).await, acknowledgement(NUM_RESULTS));
        let mut results = Vec::new();
        for _ in 0..NUM_RESULTS {
            let result_event = events.next_event().await.expect("a result should arrive");
            assert_eq!(result_event.name, None);
            results.push(result_event.data);
        }

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
    }

    #[tokio::test]
    async fn arrow_response_of_an_untracked_job_ends_with_a_null_summary() {
        let harness = Harness::start().await;
        let response = post_arrow_query_stream(&harness.app, &json!({"query_string": "*"})).await;
        harness
            .query_job_table
            .set_status(1, QueryJobStatus::Running);
        harness.query_job_table.fail_status_polls();
        let (rows, summary) = decode_arrow_body(&body_bytes(response).await);

        assert_eq!(rows, []);
        assert_eq!(
            summary,
            ArrowSummary {
                status: None,
                num_results_emitted: None,
                num_duplicates_dropped: None,
                num_protocol_errors: None,
                num_incomplete_connections: None,
            }
        );
        assert_eq!(
            harness.query_job_table.wait_for_cancel_requests().await,
            [1]
        );
    }
}
