/* EliteSQL stable C ABI — v0.0.1
 *
 * Conventions:
 *  - Every function returns uint32_t: 0 = OK, otherwise a stable error code:
 *      1 io, 2 corrupt, 3 table_exists, 4 table_not_found, 5 record_not_found,
 *      6 duplicate_id, 7 schema_violation, 8 invalid_argument,
 *      9 conflict_retry, 10 database_locked, 11 unique_violation, 12 sql,
 *      13 read_only, 14 column_not_found, 15 index_not_found, 16 memory_limit,
 *      100 internal_panic.
 *  - Output strings are heap-allocated UTF-8 JSON; free with
 *    elitesql_free_string(). elitesql_last_error() is thread-local and NOT freed.
 *  - The handle is thread-safe: readers never block writers; writers only
 *    meet at commit.
 */
#ifndef ELITESQL_H
#define ELITESQL_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct EliteSql EliteSql;

/* Static version string; do not free. */
const char *elitesql_version(void);

/* Thread-local message for the last failing call; do not free. */
const char *elitesql_last_error(void);

void elitesql_free_string(char *s);

/* Open (creating if missing). options_json: NULL or
 * {"durability": "safe"|"balanced"|"fast", "read_only"?:bool,
 *  "memory"?: {"total_memory_bytes", "query_pool_bytes",
 *    "query_working_bytes", "index_delta_pool_bytes",
 *    "maintenance_pool_bytes", "reserved_memory_bytes", "scan_batch_rows",
 *    "spill_directory"?}}. */
uint32_t elitesql_open(const char *path, const char *options_json, EliteSql **out);

uint32_t elitesql_close(EliteSql *db);

/* One SQL statement. result_json:
 * {"columns":[...],"rows":[[...]]} | {"inserted":[ids]} | {"affected":n} |
 * {"ok":true}. Non-JSON-native values are tagged: {"$t":"timestamp","us":...},
 * {"$t":"date","iso":...}, {"$t":"time","us":...}, {"$t":"blob","hex":...},
 * {"$t":"json","v":...}, {"$t":"vector","v":[...]}, or input-only
 * {"$t":"int64","v":"..."}. */
uint32_t elitesql_query(EliteSql *db, const char *sql, char **result_json);

/* Execute SQL with parameters supplied separately. params_json is an array
 * for positional ?/%s placeholders or an object for %(name)s placeholders.
 * Values use the same native/tagged representation documented above. */
uint32_t elitesql_query_params(EliteSql *db, const char *sql,
                              const char *params_json, char **result_json);

/* ANN search. params_json: {"table","column","vector":[...],"top_k",
 * "ef_search"?, "filter"?:{col:value}}. result_json:
 * {"hits":[{"id","distance","record":{...}}]}. */
uint32_t elitesql_search_vector(EliteSql *db, const char *params_json,
                              char **result_json);

/* Create an ANN index. params_json: {"table","column",
 * "metric"?: "cosine"|"dot"|"l2", "mode"?: "sync"|"async",
 * "m"?, "ef_construction"?, "quantized"?: bool}. */
uint32_t elitesql_create_vector_index(EliteSql *db, const char *params_json);

/* Full-text (BM25). create: {"table","column"}. search: {"table","column",
 * "query","top_k"?,"filter"?} -> {"hits":[{"id","score","record"}]}. */
uint32_t elitesql_create_text_index(EliteSql *db, const char *params_json);
uint32_t elitesql_search_text(EliteSql *db, const char *params_json,
                            char **result_json);

/* Hybrid RRF search: {"table","top_k"?,"ef_search"?,"filter"?,
 * "text"?: {"column","query"}, "vector"?: {"column","vector":[...]}}. */
uint32_t elitesql_search_hybrid(EliteSql *db, const char *params_json,
                              char **result_json);

/* Stable read snapshots: reads through a snapshot see the database exactly
 * as it was when the snapshot was taken. */
typedef struct EliteSqlSnapshot EliteSqlSnapshot;
uint32_t elitesql_snapshot_open(EliteSql *db, EliteSqlSnapshot **out);
uint32_t elitesql_snapshot_close(EliteSqlSnapshot *snap);
uint32_t elitesql_snapshot_get(EliteSql *db, EliteSqlSnapshot *snap, const char *table,
                             const char *id, char **result_json);
uint32_t elitesql_snapshot_scan(EliteSql *db, EliteSqlSnapshot *snap,
                              const char *table, char **result_json);

uint32_t elitesql_checkpoint(EliteSql *db);
uint32_t elitesql_compact(EliteSql *db);

/* Offline checks (db must not be open elsewhere). */
uint32_t elitesql_check(const char *path, char **report_json);
uint32_t elitesql_repair(const char *src, const char *dst, char **report_json);

#ifdef __cplusplus
}
#endif

#endif /* ELITESQL_H */
