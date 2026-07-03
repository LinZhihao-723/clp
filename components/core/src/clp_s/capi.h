#ifndef CLP_S_CAPI_H
#define CLP_S_CAPI_H

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Callback invoked once per matching search result. `message` is the NUL-terminated JSON
 * serialization of the matching record, valid only for the duration of the call. `user_data` is
 * forwarded verbatim from `clp_s_search`.
 */
typedef void (*ClpSSearchResultCallback)(char const* message, void* user_data);

/**
 * Compresses newline-delimited JSON log file(s) at `input_path` into multi-file clp-s archive(s)
 * under `archives_dir`.
 *
 * @param input_path
 * @param archives_dir
 * @return 0 on success, non-zero on failure.
 */
int clp_s_compress(char const* input_path, char const* archives_dir);

/**
 * Searches every archive under `archives_dir` with the KQL `query`, invoking `callback` once per
 * matching record.
 *
 * @param archives_dir
 * @param query
 * @param callback Invoked once per matching record, or NULL to discard results.
 * @param user_data Forwarded verbatim to every `callback` invocation.
 * @return 0 on success, non-zero on failure.
 */
int clp_s_search(
        char const* archives_dir,
        char const* query,
        ClpSSearchResultCallback callback,
        void* user_data
);

/**
 * Searches the single clp-s archive at `archive_path` with the KQL `query`, invoking `callback`
 * once per matching record.
 *
 * @param archive_path
 * @param query
 * @param callback Invoked once per matching record, or NULL to discard results.
 * @param user_data Forwarded verbatim to every `callback` invocation.
 * @return 0 on success, non-zero on failure.
 */
int clp_s_search_archive(
        char const* archive_path,
        char const* query,
        ClpSSearchResultCallback callback,
        void* user_data
);

#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // CLP_S_CAPI_H
