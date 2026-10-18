//! The results-cache sink: connection and construction errors, insert failures as sink errors,
//! and (with a `MongoDB` server, ignored by default; see `scripts/run-mongo-tests.sh`) documents
//! equal to the C++ results-cache oracles, session-wide top-k, batching and idempotent retries.

use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::LatestFirstKey;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReadError;
use clp_s_reader::ReaderContext;
use clp_s_reader::RecordId;
use clp_s_reader::ResultOrder;
use clp_s_reader::ScanRequest;
use clp_s_reader::SearchSession;
use clp_s_reader::SessionReport;
use clp_s_reader::SinkError;
use clp_s_results_cache::MongoDatabase;
use clp_s_results_cache::ResultsCacheConfig;
use clp_s_results_cache::ResultsCacheError;
use clp_s_results_cache::ResultsCacheSink;
use clp_s_results_cache::connect_database;
use mongodb::bson::Document;

/// The environment variable naming the test server's URI (with a database); the default matches
/// `scripts/run-mongo-tests.sh`.
const MONGODB_URI_VARIABLE: &str = "CLP_S_TEST_MONGODB_URI";

const DEFAULT_MONGODB_URI: &str = "mongodb://127.0.0.1:27918/proto";

/// One results-cache document as comparable fields:
/// `(archive_id, log_event_idx, dataset, message, orig_file_path, timestamp)`.
type Fields = (String, i64, String, String, String, i64);

/// # Returns
///
/// The prototype root.
fn prototype_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// # Returns
///
/// The path of fixture archive `name`.
fn fixture(name: &str) -> PathBuf {
    prototype_root()
        .join("tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// # Returns
///
/// A multi-thread runtime for the sink.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime can be built")
}

/// # Returns
///
/// The report of a session running `kql` in `order` over the fixtures `names` into `collection`
/// of `database`, on success.
///
/// # Errors
///
/// Returns an error if the session fails.
fn run_session(
    runtime: &tokio::runtime::Runtime,
    database: MongoDatabase,
    collection: &str,
    names: &[&str],
    kql: &str,
    order: ResultOrder,
    config: ResultsCacheConfig,
) -> anyhow::Result<Result<SessionReport<u64>, ReadError>> {
    let sink = ResultsCacheSink::new(runtime.handle().clone(), database, collection, "", config)?;
    let context = ReaderContext::builder().build()?;
    let request = ScanRequest::builder()
        .query(Query::from_kql(kql, &QueryOptions::default())?)
        .order(order)
        .build()?;
    Ok(names
        .iter()
        .fold(
            SearchSession::builder(context, Arc::new(request)),
            |builder, name| {
                builder.archive(
                    ArchiveId::from(format!("{name}.clps")),
                    ArchiveLocation::File(fixture(name)),
                )
            },
        )
        .build()?
        .run(Arc::new(sink)))
}

/// # Returns
///
/// The fields of a stored document.
fn stored_fields(document: &Document) -> Fields {
    let id = document
        .get_document("_id")
        .expect("the document has an _id");
    let text = |document: &Document, key: &str| {
        document
            .get_str(key)
            .expect("the document has the string field")
            .to_owned()
    };
    let integer = |document: &Document, key: &str| {
        document
            .get_i64(key)
            .expect("the document has the int64 field")
    };
    (
        text(id, "archive_id"),
        integer(id, "log_event_idx"),
        text(document, "dataset"),
        text(document, "message"),
        text(document, "orig_file_path"),
        integer(document, "timestamp"),
    )
}

/// # Returns
///
/// The sorted fields of the C++ results-cache oracle `id` (`MongoDB` canonical extended JSON).
fn oracle_fields(id: &str) -> Vec<Fields> {
    let text = std::fs::read_to_string(
        prototype_root()
            .join("tests/queries/oracles/results-cache")
            .join(format!("{id}.json")),
    )
    .expect("the results-cache oracle is readable");
    let documents: Vec<serde_json::Value> =
        serde_json::from_str(&text).expect("the oracle is a JSON array");
    let mut fields: Vec<Fields> = documents
        .iter()
        .map(|document| {
            let integer = |value: &serde_json::Value| {
                value["$numberLong"]
                    .as_str()
                    .and_then(|number| number.parse::<i64>().ok())
                    .expect("an extended-JSON int64")
            };
            let string =
                |value: &serde_json::Value| value.as_str().expect("a JSON string").to_owned();
            (
                string(&document["_id"]["archive_id"]),
                integer(&document["_id"]["log_event_idx"]),
                string(&document["dataset"]),
                string(&document["message"]),
                string(&document["orig_file_path"]),
                integer(&document["timestamp"]),
            )
        })
        .collect();
    fields.sort();
    fields
}

/// # Returns
///
/// The test server's database, connected through [`connect_database`].
fn test_database(runtime: &tokio::runtime::Runtime) -> MongoDatabase {
    let uri =
        std::env::var(MONGODB_URI_VARIABLE).unwrap_or_else(|_| DEFAULT_MONGODB_URI.to_owned());
    connect_database(runtime.handle(), &uri).expect("the test URI is valid")
}

/// # Returns
///
/// The sorted fields of every document of `collection`, which is dropped afterwards.
fn take_collection(
    runtime: &tokio::runtime::Runtime,
    database: &MongoDatabase,
    collection: &str,
) -> Vec<Fields> {
    let collection = database.collection::<Document>(collection);
    let mut fields = runtime.block_on(async {
        let mut cursor = collection
            .find(Document::new())
            .await
            .expect("the collection can be read");
        let mut fields = Vec::new();
        while cursor.advance().await.expect("the cursor advances") {
            fields.push(stored_fields(
                &cursor
                    .deserialize_current()
                    .expect("the document deserializes"),
            ));
        }
        collection
            .drop()
            .await
            .expect("the collection can be dropped");
        fields
    });
    fields.sort();
    fields
}

/// # Returns
///
/// A collection name unique to this test process and `test`.
fn collection_name(test: &str) -> String {
    format!("{test}_{}", std::process::id())
}

#[test]
fn connections_validate_the_runtime_and_the_uri() {
    let current_thread = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime can be built");
    assert!(matches!(
        connect_database(current_thread.handle(), "mongodb://127.0.0.1:1/proto"),
        Err(ResultsCacheError::CurrentThreadRuntime)
    ));
    let runtime = runtime();
    for (uri, is_invalid) in [("not a uri", true), ("mongodb://127.0.0.1:1", false)] {
        let result = connect_database(runtime.handle(), uri);
        if is_invalid {
            assert!(
                matches!(result, Err(ResultsCacheError::InvalidUri(_))),
                "{uri}: {result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(ResultsCacheError::MissingDatabase)),
                "{uri}: {result:?}"
            );
        }
    }
    let database = connect_database(runtime.handle(), "mongodb://127.0.0.1:1/proto")
        .expect("the connection is lazy");
    assert_eq!(database.name(), "proto");

    let sink = ResultsCacheSink::new(
        current_thread.handle().clone(),
        database.clone(),
        "results",
        "",
        ResultsCacheConfig::default(),
    );
    assert!(matches!(sink, Err(ResultsCacheError::CurrentThreadRuntime)));
    let sink = ResultsCacheSink::new(
        runtime.handle().clone(),
        database,
        "results",
        "",
        ResultsCacheConfig { batch_size: 0 },
    );
    assert!(matches!(sink, Err(ResultsCacheError::InvalidBatchSize)));
}

#[test]
fn unreachable_servers_fail_the_session_with_a_database_error() -> anyhow::Result<()> {
    let runtime = runtime();
    let database = connect_database(
        runtime.handle(),
        "mongodb://127.0.0.1:1/proto?serverSelectionTimeoutMS=200",
    )?;
    let error = run_session(
        &runtime,
        database,
        "results",
        &["search.ts"],
        "level: WARN",
        ResultOrder::Scan,
        ResultsCacheConfig::default(),
    )?
    .expect_err("the insert cannot reach the server");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Database(_))),
        "{error:?}"
    );
    Ok(())
}

#[test]
#[ignore = "needs a MongoDB server (scripts/run-mongo-tests.sh)"]
fn top_k_documents_equal_the_cpp_oracle() -> anyhow::Result<()> {
    let runtime = runtime();
    let database = test_database(&runtime);
    let collection = collection_name("wire_cache_topk");
    let report = run_session(
        &runtime,
        database.clone(),
        &collection,
        &["logs.ts"],
        "*",
        ResultOrder::LatestFirst {
            limit: NonZeroU32::new(10).expect("non-zero"),
        },
        ResultsCacheConfig::default(),
    )??;
    assert_eq!(report.output, 10);
    assert_eq!(
        take_collection(&runtime, &database, &collection),
        oracle_fields("wire-cache-topk")
    );
    Ok(())
}

#[test]
#[ignore = "needs a MongoDB server (scripts/run-mongo-tests.sh)"]
fn multi_archive_top_k_is_session_wide() -> anyhow::Result<()> {
    let runtime = runtime();
    let database = test_database(&runtime);
    let names: Vec<String> = (0..9)
        .map(|index| format!("logs.ts-split.{index}"))
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let limit = 10;
    let collection = collection_name("wire_cache_split");
    run_session(
        &runtime,
        database.clone(),
        &collection,
        &names,
        "level: ERROR",
        ResultOrder::LatestFirst {
            limit: NonZeroU32::new(limit).expect("non-zero"),
        },
        ResultsCacheConfig::default(),
    )??;
    let top_k = take_collection(&runtime, &database, &collection);

    let all = collection_name("wire_cache_split_all");
    run_session(
        &runtime,
        database.clone(),
        &all,
        &names,
        "level: ERROR",
        ResultOrder::Scan,
        ResultsCacheConfig::default(),
    )??;
    let mut expected = take_collection(&runtime, &database, &all);
    expected.sort_by_key(|fields| LatestFirstKey {
        timestamp_ms: Some(fields.5),
        archive_id: ArchiveId::from(fields.0.as_str()),
        record_id: RecordId::new(fields.1),
    });
    expected.truncate(limit as usize);
    expected.sort();
    assert_eq!(top_k, expected);

    let cpp = oracle_fields("wire-cache-split");
    assert!(
        cpp.len() > top_k.len(),
        "C++ keeps a top-k per archive (DV-25)"
    );
    for fields in &top_k {
        assert!(cpp.contains(fields), "{fields:?}");
    }
    Ok(())
}

#[test]
#[ignore = "needs a MongoDB server (scripts/run-mongo-tests.sh)"]
fn ids_do_not_collide_without_log_order() -> anyhow::Result<()> {
    let runtime = runtime();
    let database = test_database(&runtime);
    let collection = collection_name("wire_cache_nolog");
    run_session(
        &runtime,
        database.clone(),
        &collection,
        &["search.ts-nolog"],
        "level: INFO",
        ResultOrder::LatestFirst {
            limit: NonZeroU32::new(10).expect("non-zero"),
        },
        ResultsCacheConfig::default(),
    )??;
    let documents = take_collection(&runtime, &database, &collection);
    let cpp = oracle_fields("wire-cache-nolog");
    assert_eq!(cpp.len(), 1, "C++ `_id`s collide (DV-24)");
    assert_eq!(documents.len(), 10);
    let mut ids: Vec<i64> = documents.iter().map(|fields| fields.1).collect();
    ids.dedup();
    assert_eq!(ids.len(), 10, "{documents:?}");
    assert!(
        documents.iter().any(|fields| fields.3 == cpp[0].3),
        "{documents:?}"
    );
    Ok(())
}

#[test]
#[ignore = "needs a MongoDB server (scripts/run-mongo-tests.sh)"]
fn batches_are_inserted_and_retries_are_idempotent() -> anyhow::Result<()> {
    let runtime = runtime();
    let database = test_database(&runtime);
    let collection = collection_name("batches");
    for _ in 0..2 {
        let report = run_session(
            &runtime,
            database.clone(),
            &collection,
            &["logs.ts"],
            "*",
            ResultOrder::Scan,
            ResultsCacheConfig { batch_size: 7 },
        )??;
        assert_eq!(report.output, 3000);
    }
    let documents = take_collection(&runtime, &database, &collection);
    assert_eq!(documents.len(), 3000);
    let mut expected_messages: Vec<String> =
        std::fs::read_to_string(prototype_root().join("tests/fixtures/oracles/logs.ts.jsonl"))?
            .lines()
            .map(|line| format!("{line}\n"))
            .collect();
    expected_messages.sort();
    let mut messages: Vec<String> = documents.into_iter().map(|fields| fields.3).collect();
    messages.sort();
    assert_eq!(messages, expected_messages);
    Ok(())
}
