/* ClawDB stable C ABI — v0.0.1
 *
 * Conventions:
 *  - Every function returns uint32_t: 0 = OK, otherwise a stable error code:
 *      1 io, 2 corrupt, 3 table_exists, 4 table_not_found, 5 record_not_found,
 *      6 duplicate_id, 7 schema_violation, 8 invalid_argument,
 *      9 conflict_retry, 10 database_locked, 11 unique_violation, 12 sql,
 *      100 internal_panic.
 *  - Output strings are heap-allocated UTF-8 JSON; free with
 *    clawdb_free_string(). clawdb_last_error() is thread-local and NOT freed.
 *  - The handle is thread-safe: readers never block writers; writers only
 *    meet at commit.
 */
#ifndef CLAWDB_H
#define CLAWDB_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ClawDb ClawDb;

/* Static version string; do not free. */
const char *clawdb_version(void);

/* Thread-local message for the last failing call; do not free. */
const char *clawdb_last_error(void);

void clawdb_free_string(char *s);

/* Open (creating if missing). options_json: NULL or
 * {"durability": "safe"|"balanced"|"fast"}. */
uint32_t clawdb_open(const char *path, const char *options_json, ClawDb **out);

uint32_t clawdb_close(ClawDb *db);

/* One SQL statement. result_json:
 * {"columns":[...],"rows":[[...]]} | {"inserted":[ids]} | {"affected":n} |
 * {"ok":true}. Non-JSON-native values are tagged: {"$t":"timestamp","us":...},
 * {"$t":"date","iso":...}, {"$t":"time","us":...}, {"$t":"blob","hex":...},
 * {"$t":"json","v":...}, {"$t":"vector","v":[...]}. */
uint32_t clawdb_query(ClawDb *db, const char *sql, char **result_json);

/* ANN search. params_json: {"table","column","vector":[...],"top_k",
 * "ef_search"?, "filter"?:{col:value}}. result_json:
 * {"hits":[{"id","distance","record":{...}}]}. */
uint32_t clawdb_search_vector(ClawDb *db, const char *params_json,
                              char **result_json);

/* Create an ANN index. params_json: {"table","column",
 * "metric"?: "cosine"|"dot"|"l2", "mode"?: "sync"|"async",
 * "m"?, "ef_construction"?, "quantized"?: bool}. */
uint32_t clawdb_create_vector_index(ClawDb *db, const char *params_json);

/* Full-text (BM25). create: {"table","column"}. search: {"table","column",
 * "query","top_k"?,"filter"?} -> {"hits":[{"id","score","record"}]}. */
uint32_t clawdb_create_text_index(ClawDb *db, const char *params_json);
uint32_t clawdb_search_text(ClawDb *db, const char *params_json,
                            char **result_json);

/* Hybrid RRF search: {"table","top_k"?,"ef_search"?,"filter"?,
 * "text"?: {"column","query"}, "vector"?: {"column","vector":[...]}}. */
uint32_t clawdb_search_hybrid(ClawDb *db, const char *params_json,
                              char **result_json);

/* Stable read snapshots: reads through a snapshot see the database exactly
 * as it was when the snapshot was taken. */
typedef struct ClawSnapshot ClawSnapshot;
uint32_t clawdb_snapshot_open(ClawDb *db, ClawSnapshot **out);
uint32_t clawdb_snapshot_close(ClawSnapshot *snap);
uint32_t clawdb_snapshot_get(ClawDb *db, ClawSnapshot *snap, const char *table,
                             const char *id, char **result_json);
uint32_t clawdb_snapshot_scan(ClawDb *db, ClawSnapshot *snap,
                              const char *table, char **result_json);

uint32_t clawdb_checkpoint(ClawDb *db);
uint32_t clawdb_compact(ClawDb *db);

/* Offline checks (db must not be open elsewhere). */
uint32_t clawdb_check(const char *path, char **report_json);
uint32_t clawdb_repair(const char *src, const char *dst, char **report_json);

#ifdef __cplusplus
}
#endif

#endif /* CLAWDB_H */
