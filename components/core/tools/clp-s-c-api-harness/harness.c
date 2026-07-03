/**
 * Self-contained harness that, in a SINGLE process, compresses sample JSON logs into a clp-s
 * archive and then searches it using ONLY the C API exported by libclp-s.so. No shelling out to
 * the clp-s binary: both compression and search go through `clp_s_compress` / `clp_s_search`.
 */

#include <clp_s/capi.h>

#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

/**
 * State threaded through `clp_s_search` via `user_data`. Counts the matching records and stashes
 * up to `kMaxStashedResults` of the matched JSON strings for inspection.
 */
enum { kMaxStashedResults = 16 };

typedef struct {
    int count;
    char* results[kMaxStashedResults];
} SearchAccumulator;

/**
 * Search callback: invoked once per matching record. Increments the match count and stashes a copy
 * of the record's JSON serialization (bounded by `kMaxStashedResults`).
 */
static void accumulate_result(char const* message, void* user_data) {
    SearchAccumulator* acc = (SearchAccumulator*)user_data;
    if (acc->count < kMaxStashedResults && NULL != message) {
        acc->results[acc->count] = strdup(message);
    }
    ++acc->count;
}

static void reset_accumulator(SearchAccumulator* acc) {
    for (int i = 0; i < acc->count && i < kMaxStashedResults; ++i) {
        free(acc->results[i]);
        acc->results[i] = NULL;
    }
    acc->count = 0;
}

/**
 * Runs a single KQL query, compares the observed match count against the expected count, and prints
 * a PASS/FAIL line.
 * @return 1 if the observed count matches the expected count, 0 otherwise.
 */
static int run_query_test(
        char const* archives_dir,
        char const* query,
        int expected_count
) {
    SearchAccumulator acc = {0};
    int const rc = clp_s_search(archives_dir, query, accumulate_result, &acc);
    if (0 != rc) {
        printf("FAIL | rc=%d (search returned non-zero) | query: %s\n", rc, query);
        reset_accumulator(&acc);
        return 0;
    }

    int const passed = (acc.count == expected_count);
    printf(
            "%s | expected=%d actual=%d | query: %s\n",
            passed ? "PASS" : "FAIL",
            expected_count,
            acc.count,
            query
    );
    if (0 == passed) {
        for (int i = 0; i < acc.count && i < kMaxStashedResults; ++i) {
            printf("       matched: %s\n", acc.results[i]);
        }
    }
    reset_accumulator(&acc);
    return passed;
}

/**
 * Runs a single KQL query against one specific archive via `clp_s_search_archive`, compares the
 * observed match count against the expected count, and prints a PASS/FAIL line.
 * @return 1 if the observed count matches the expected count, 0 otherwise.
 */
static int run_single_archive_query_test(
        char const* archive_path,
        char const* query,
        int expected_count
) {
    SearchAccumulator acc = {0};
    int const rc = clp_s_search_archive(archive_path, query, accumulate_result, &acc);
    if (0 != rc) {
        printf("FAIL | rc=%d (search_archive returned non-zero) | query: %s\n", rc, query);
        reset_accumulator(&acc);
        return 0;
    }

    int const passed = (acc.count == expected_count);
    printf(
            "%s | expected=%d actual=%d | query: %s\n",
            passed ? "PASS" : "FAIL",
            expected_count,
            acc.count,
            query
    );
    reset_accumulator(&acc);
    return passed;
}

/**
 * Discovers the path to the single archive subdirectory under `archives_dir` by taking the first
 * non-dot directory entry. Writes the full path into `archive_path` (of size `archive_path_size`).
 * @return 1 on success, 0 if no archive subdirectory is found.
 */
static int discover_single_archive_path(
        char const* archives_dir,
        char* archive_path,
        size_t archive_path_size
) {
    DIR* dir = opendir(archives_dir);
    if (NULL == dir) {
        fprintf(stderr, "Failed to open archives directory: %s\n", strerror(errno));
        return 0;
    }

    int found = 0;
    struct dirent* entry = NULL;
    while (NULL != (entry = readdir(dir))) {
        if (0 == strcmp(entry->d_name, ".") || 0 == strcmp(entry->d_name, "..")) {
            continue;
        }
        snprintf(archive_path, archive_path_size, "%s/%s", archives_dir, entry->d_name);
        struct stat st;
        if (0 == stat(archive_path, &st) && S_ISDIR(st.st_mode)) {
            found = 1;
            break;
        }
    }
    closedir(dir);
    return found;
}

int main(int argc, char** argv) {
    char const* input_path
            = (argc > 1) ? argv[1] : "sample_logs.jsonl";
    char const* archives_dir
            = (argc > 2) ? argv[2] : "./harness-work/archives";

    // Start from a clean archives directory so runs are repeatable.
    char cmd[4096];
    snprintf(cmd, sizeof(cmd), "rm -rf '%s'", archives_dir);
    if (0 != system(cmd)) {
        fprintf(stderr, "Failed to clean archives directory: %s\n", archives_dir);
        return 2;
    }
    // Create the archives directory (and its parent). clp_s_compress needs it to exist. Ignore
    // EEXIST since the parent may already be present.
    if (0 != mkdir("./harness-work", 0755) && EEXIST != errno) {
        fprintf(stderr, "Failed to create ./harness-work: %s\n", strerror(errno));
        return 2;
    }
    if (0 != mkdir(archives_dir, 0755) && EEXIST != errno) {
        fprintf(stderr, "Failed to create %s: %s\n", archives_dir, strerror(errno));
        return 2;
    }

    printf("=== clp-s C API harness ===\n");
    printf("input:        %s\n", input_path);
    printf("archives_dir: %s\n\n", archives_dir);

    printf("--- compress (clp_s_compress) ---\n");
    int const compress_rc = clp_s_compress(input_path, archives_dir);
    printf("clp_s_compress returned %d\n\n", compress_rc);
    if (0 != compress_rc) {
        fprintf(stderr, "Compression failed; aborting.\n");
        return 3;
    }

    // Each test: {query, expected_count}. Expected counts are derived by hand from
    // sample_logs.jsonl (10 records, ids 1..10).
    struct {
        char const* query;
        int expected;
        char const* kind;
    } tests[] = {
            // Exact-match string queries.
            {"level: \"ERROR\"", 3, "exact"},
            {"service: \"auth\"", 4, "exact"},
            // Wildcard substring queries.
            {"message: \"*timeout*\"", 2, "wildcard"},
            {"message: \"*failed*\"", 3, "wildcard"},
            // Numeric comparisons.
            {"id > 7", 3, "numeric"},
            {"id > 4 AND id < 7", 2, "numeric-range"},
            // Boolean combinations.
            {"level: \"ERROR\" AND service: \"payment\"", 1, "AND"},
            {"level: \"WARN\" OR level: \"ERROR\"", 5, "OR"},
            {"service: \"gateway\" AND NOT level: \"ERROR\"", 2, "NOT"},
            // Deliberate zero-match queries.
            {"level: \"FATAL\"", 0, "zero-match"},
            {"id > 100", 0, "zero-match"},
    };
    int const num_tests = (int)(sizeof(tests) / sizeof(tests[0]));

    printf("--- search (clp_s_search) ---\n");
    int passes = 0;
    for (int i = 0; i < num_tests; ++i) {
        passes += run_query_test(archives_dir, tests[i].query, tests[i].expected);
    }

    printf("\n=== summary: %d/%d queries passed ===\n", passes, num_tests);

    // Discover the single archive produced by the one compress above, then exercise
    // `clp_s_search_archive` against that specific archive path. The counts must match those
    // returned by the directory-based `clp_s_search` for the same queries.
    printf("\n--- search single archive (clp_s_search_archive) ---\n");
    char archive_path[4096];
    if (0 == discover_single_archive_path(archives_dir, archive_path, sizeof(archive_path))) {
        fprintf(stderr, "Failed to discover a single archive under %s\n", archives_dir);
        return 4;
    }
    printf("archive_path: %s\n", archive_path);

    int single_passes = 0;
    for (int i = 0; i < num_tests; ++i) {
        single_passes += run_single_archive_query_test(
                archive_path,
                tests[i].query,
                tests[i].expected
        );
    }
    printf(
            "\n=== single-archive summary: %d/%d queries passed ===\n",
            single_passes,
            num_tests
    );

    if (passes != num_tests || single_passes != num_tests) {
        printf("RESULT: FAIL\n");
        return 1;
    }
    printf("RESULT: PASS\n");
    return 0;
}
