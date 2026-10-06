# Using the API server

CLP includes an API server that provides a RESTful interface for interacting with CLP.

## Starting the API server

CLP starts the API server based on the `api_server` section in `etc/clp-config.yaml`, which includes
a default configuration. You can uncomment and modify this section to override the defaults.

## API reference

All available API endpoints are defined in the [OpenAPI] Specification. You can explore the API
using [Swagger UI][swagger-ui].

:::{note}
`clp-text` doesn't support buffering search results in the file system or S3. When using
`clp-text`, set `buffer_results_in_mongodb` to `true` so that results are buffered in `MongoDB`
instead.
:::

## Example: Submitting search queries and receiving results

The API server exposes endpoints to submit search queries, and returns search results as a
continuous stream using [Server-sent Events][server-sent-events].

Assuming the server is running on the default host and port (`localhost:3001`), you can use the
following commands to submit a query to clp-json and stream the results.

1. Submit a search query:

   ```shell
   curl -X POST http://localhost:3001/query \
       -H "Content-Type: application/json" \
       -d '{
          "query_string": "*log*",
          "datasets": ["default"],
          "ignore_case": false,
          "max_num_results": 100
       }'
   ```

   On success, the server responds with:

    ```json
    {
      "query_results_uri": "query_results/100"
    }
    ```

2. Retrieve search results:
   Use the returned `query_results_uri` to receive search results as an SSE stream:

   ```bash
   curl -N http://localhost:3001/query_results/100
   ```

   Example streamed output:

   ```text
   data: {"timestamp": 1767225600000, "message": "Example log message"}

   data: {"timestamp": 1767225600010, "message": "Another matched log line"}

   data: {"timestamp": 1767225600020, "message": "No logs found" }
   ```

## Example: Streaming search results in a single request

When `package.scheduler` is set to `spider` in `etc/clp-config.yaml`, the API server can also submit
a search query and stream its results back in the same request, as the search tasks find them:

```shell
curl -N -X POST http://localhost:3001/query/stream \
    -H "Content-Type: application/json" \
    -d '{
       "query_string": "*log*",
       "datasets": ["default"],
       "ignore_case": false
    }'
```

The response is a stream of [Server-sent Events][server-sent-events]: a `job` event with the query
job's ID, one event per result, and an `end` event with the job's terminal status once the job has
finished:

```text
event: job
data: {"query_job_id":100}

data: {"archive_id":"018e90e5-8b2a-4a61-a2fc-cac799936caf","timestamp":1767225600000,"message":"Example log message"}

event: end
data: {"status":"Succeeded","num_results_emitted":1,"num_duplicates_dropped":0,"num_protocol_errors":0}
```

Results from different archives arrive in no particular order. Streaming search doesn't support
`max_num_results` (other than `0`), `buffer_results_in_mongodb`, or
`count_by_time_bucket_size_millisecs`. If the client disconnects before the `end` event, the query
job is marked for cancellation. If the API server can no longer track the job's status, an `error`
event replaces the `end` event, and the query job is also marked for cancellation.

[OpenAPI]: https://swagger.io/specification/
[server-sent-events]: https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events
[swagger-ui]: https://petstore.swagger.io/?url=https://docs.yscope.com/clp/DOCS_VAR_CLP_GIT_REF/_static/generated/api-server-openapi.json
