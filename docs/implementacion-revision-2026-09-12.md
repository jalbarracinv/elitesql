# Implementación de la tercera revisión de integridad (R01–R19)

Fecha: 2026-09-12. Base: `3f7267b1e50a0a0b62e0ae185e80a5fa14b449ce`. Los
cambios están en el árbol de trabajo, sin commit. Este documento acompaña a
[revision-integridad-2026-09-12.md](revision-integridad-2026-09-12.md).

Criterio de diseño de todas las correcciones: ninguna añade trabajo a la ruta
caliente de commit más allá de comprobaciones de coste constante (una búsqueda
en un mapa que ya se hacía, una comparación de punto flotante, un `bool`). El
coste nuevo se concentra en apertura, cierre, recuperación y herramientas.

## Correcciones

| ID | Cambio | Dónde | Prueba |
|---|---|---|---|
| R01 | La apertura con `ADD COLUMN` pendiente carga los índices derivados como una apertura normal (sólo los intents que reescriben segmentos los reconstruyen ellos mismos). `finish_ddl_recovery` comprueba que cada índice del catálogo existe en memoria y reconstruye si falta. `validate_unique` e `INSERT IGNORE` devuelven `Corrupt` ante un índice único declarado pero ausente, en vez de omitir la comprobación | `db.rs` (`open_with`, `finish_ddl_recovery`, `derived_indexes_complete`, `insert_if_unique`), `db/commit.rs` (`validate_unique`, `missing_unique_index`) | `integrity_regressions::recovery_of_a_pending_add_column_keeps_every_derived_index_enforced` |
| R02 | `scan_wal` clasifica como cola incompleta (truncable) cualquier fallo de parseo o CRC sin un registro válido posterior, incluidas colas rellenas de ceros o basura; sólo un registro válido posterior convierte el daño en corrupción. `validate_wal_chain` acepta una cola incompleta antes de sucesores mientras éstos no contengan commits | `wal.rs` | `integrity_regressions::zero_filled_and_garbage_wal_tails_are_torn_tails_not_corruption`, `torn_wal_before_empty_reserved_successors_is_recoverable`; la prueba existente de corrupción interior sigue pasando |
| R03 | Un `fsync` fallido del WAL (líder de grupo, lote coordinado o temporizador) valla las escrituras hasta reabrir, igual que una publicación de manifest incierta | `db/commit.rs` (`fence_after_wal_sync_failure`) | `sync_failure_is_reported_as_unknown_after_logical_publication`, `coordinated_safe_sync_failure_is_reported_to_every_batch_member` (actualizadas: el siguiente commit recibe `CommitUnknown`; tras reabrir se escribe con normalidad) |
| R04 | `load_vector_run_set` devuelve `floor = min(generación del primer run, generación del manifest)` | `db.rs` | `vector_run_set_catch_up_starts_at_the_manifest_generation` |
| R05 | El cliente Python del sidecar envía `id` en cada petición y exige que la respuesta lo devuelva; timeout, EOF o id distinto cierran la conexión y el cliente rechaza cualquier uso posterior con código 1 y mensaje explícito | `bindings/python/elitesql.py` (`_exchange`, `_break`) | `tests/test_params.py::SidecarRequestIdTests` |
| R06 | Códigos y política de reintento documentados; constantes para 1, 2, 9, 10, 11, 13, 16, 17, 18, 20, 21 en Python y Node; propiedades `retry_safe`/`maybe_published`; el vencimiento de transacción del sidecar devuelve el código 21 propio en lugar de 8; protocolo documentado en `serve.rs` | `bindings/python/README.md`, `elitesql.py`, `bindings/node/elitesql.js`, `.d.ts`, `crates/elitesql-cli/src/serve.rs` | pruebas unitarias de ambos bindings |
| R07 | `exec_update_txn` toma la clave física de la tupla `(id, record)` de `Txn::scan` | `sql/exec.rs` | `transactional_update_works_with_a_declared_integer_primary_key` |
| R08 | `TableSchema::validate` rechaza nombres con caracteres de control (cubre el prefijo `\0` de la pseudo-tabla de identidades) | `schema.rs` | `table_names_with_control_characters_are_rejected` |
| R09 | `ADD COLUMN ... NOT NULL` verifica bajo el mutex de commit que no queda ningún NULL antes de publicar; si lo hay, repite el backfill (acotado) o falla con `Conflict`; con NULL sin default falla con `SchemaViolation` | `db.rs` (`apply_add_column`, `ids_needing_fill_in`) | Cubierta por la suite de DDL existente; la carrera en sí no es determinista y no se simula |
| R10 | `import` confirma en una sola transacción por defecto; `--batch N` para lotes, con mensaje de error que indica cuántas filas ya están confirmadas. Códigos de salida: 3 para `check`/`restore` con advertencias y `export --read-only` parcial, 4 para `repair` con entradas omitidas; advertencias a stderr | `crates/elitesql-cli/src/main.rs` | `elitesql --help` documenta los códigos |
| R11 | Node: `±inf`/`NaN` decodificados desde `repr`; timestamps conservan microsegundos (`timestampMicros(date)`, propiedad no enumerable que `encodeParam` reutiliza); valores `json` con enteros fuera de 2^53 viajan como texto y se parsean con `parseJsonExact` a `BigInt`; `close()` espera las peticiones ya enviadas en vez de rechazarlas | `bindings/node/elitesql.js`, `.d.ts`, `jsonio.rs` (`json_has_unsafe_integer`, etiqueta `{"$t":"json","text":...}`), Python decodifica la etiqueta | `bindings/node/test.js`, `test-integration.js` |
| R12 | Credenciales eliminadas de la guía (el archivo está en `.gitignore` y nunca se confirmó; se indica revisar las copias `.build-elitesql-*` del NAS), `git archive` de la revisión confirmada, paso nuevo de `check` + `backup` con la revisión a instalar y comprobación de volumen local, parada del servicio por su script esperando a que no quede ningún proceso, nota de compatibilidad de formato en la reversión | `ActualizarNAS.md` | — |
| R13 | Temporizador de sincronización en `Balanced` (sólo actúa si hay anexos sin sincronizar y el intervalo venció); el cierre limpio sincroniza el WAL en todos los modos; `DbOptions::full_fsync` (FFI `full_fsync`, CLI `--full-fsync`, Python `full_fsync=`) usa `F_FULLFSYNC` en macOS a través del módulo `durable`, aplicado a WAL, manifest, directorios, segmentos, puente WAL y blobs. Tablas de durabilidad del README y `getting-started.md` reescritas con el contrato real | `wal.rs`, `db.rs`, `durable.rs`, `manifest.rs`, `db/maintenance.rs` | `balanced_syncs_an_idle_writer_within_its_interval_and_on_close` |
| R14 | Tras el replay, si el escritor reanuda en un WAL por encima de `required_wal_id`, la apertura republica el manifest con la extensión real antes de aceptar commits | `db.rs` (`open_with`) | `recovery_records_the_wal_extent_it_resumes_in` |
| R15 | `-0.0` se canonicaliza a `0.0` al almacenar (INSERT y UPDATE), en `index_key` y en la comparación SQL; NaN se rechaza en columnas `float64` | `db.rs` (`canonical_value`, `check_value`, `normalize_record`, `Txn::update`), `sql/values.rs` | `float_zero_signs_and_nan_agree_across_indexes_equality_and_storage` |
| R16 | `DROP COLUMN` rechaza la columna identidad; un parámetro texto en columna `json` se parsea como el literal (error si no es JSON); `Int → Date` valida el rango de calendario en literal y parámetro. `drop_table` ya podaba la identidad del manifest: sin cambio | `db.rs`, `sql/values.rs`, `value.rs` (`date_days_in_range`), `manual.md` | `identity_column_cannot_be_dropped_and_json_parameters_match_literals` |
| R17 | `referenced_files` devuelve `None` cuando el manifest existe pero no se puede leer y los barridos de huérfanos no borran nada en ese caso; `publish_vector_merge` sólo desenlaza runs antiguos si publicó el manifest nuevo o no había manifest durable | `run_manifest.rs`, `db.rs` | `orphan_sweeps_skip_an_index_whose_manifest_cannot_be_read` |
| R18 | `Db::recovery_warnings()` expone lo que una apertura de sólo lectura omitió (segmento ilegible o dañado, WAL ilegible o con cola incompleta); `export --read-only` lo imprime en stderr y sale con 3 | `db.rs`, `crates/elitesql-cli/src/main.rs` | `read_only_open_reports_what_it_could_not_expose` |
| R19 | Oráculo del fuzz de corrupción endurecido: si `open` acepta, `check` debe validar; si `open` rechaza, `check` debe encontrar el error; los archivos de `indexes/` entran en el fuzz y una alteración ahí nunca puede rechazar la apertura ni perder filas. Pruebas nuevas para colas con ceros/basura, sucesores vacíos, extensión de WAL, fencing tras fsync, temporizador de `Balanced`, suelo del catch-up vectorial y barridos de huérfanos | `tests/corruption.rs`, `tests/integrity_regressions.rs`, pruebas unitarias de `db.rs` | — |

### Hallazgo adicional corregido durante la implementación

El fuzz endurecido de R19 reveló que **una alteración en el cuerpo de una
página de un índice derivado rechazaba la apertura** con `Corrupt("paged
index: page crc mismatch")` en lugar de reconstruir el índice. El CRC de
página se verificaba de forma perezosa al leer, así que el fallo salía por la
primera lectura de la apertura y se presentaba como daño de la base. Ahora
`PagedIndex::validate_pages` recorre los CRC de todas las páginas al cargar
runs primarios, secundarios y de texto (y la base primaria heredada); cualquier
fallo descarta el run y dispara la reconstrucción desde segmentos. Coste: una
pasada CRC sobre los índices al abrir (crc32 por hardware, del orden de
milisegundos por decenas de MB); no afecta a commits ni consultas.

### Dos fallos de CI detectados tras el primer push

La ejecución de `Acceptance` sobre el commit anterior y sobre `2a8ca0f` dejó
un job colgado hasta el límite de 30 minutos (Ubuntu 1.89 primero, macOS
1.93.1 después) y otro fallido por tiempos. Ambos eran preexistentes e
intermitentes; los logs de los jobs los localizan:

- **Interbloqueo en `find_eq_unbudgeted`** (`db.rs`). La función registraba un
  snapshot con el orden correcto de cerrojos (registro → estado), pero
  declaraba el guard del snapshot *después* del guard de lectura del estado,
  así que al salir soltaba el snapshot (que toma el registro) mientras aún
  sostenía el estado. Con un `Db::snapshot()` concurrente (registro tomado,
  lectura del estado encolada) y un commit esperando la escritura del estado,
  el `RwLock` de la biblioteca estándar, que prioriza al escritor, cerraba el
  ciclo. Reproducido localmente 2 veces en 150 ejecuciones de
  `concurrent_autocommit_deletes_never_conflict` y confirmado con una muestra
  de pilas del proceso colgado. Corrección: declarar el guard antes que el
  estado para que se libere después. Regla que documenta el código: nunca
  soltar un `Snapshot` con el estado tomado.
- **`a_large_scan_yields_the_state_lock_to_a_concurrent_writer`** (`tests/bulk.rs`)
  comparaba tiempos de reloj con un solo intento; en un runner de dos núcleos
  el hilo del commit compite por CPU con el del escaneo y fallaba por ruido.
  Ahora hace hasta tres intentos y exige además que el escaneo siguiera en
  curso cuando el commit terminó, que es la propiedad que importa.

## Límites que permanecen

- **Bit rot en el último registro del WAL** es indistinguible de una
  escritura torn y se trunca (misma política que PostgreSQL). Todos los
  registros anteriores conservan su protección CRC y cualquier daño seguido de
  un registro válido sigue rechazándose.
- **Pérdida de energía real** sigue sin simularse; las pruebas nuevas
  reproducen sus efectos típicos (ceros, basura, cola torn con sucesores) sobre
  archivos, no el comportamiento del dispositivo.
- **Write skew** sigue siendo un límite documentado del aislamiento snapshot.
- **R09** corrige la ventana por construcción; la carrera no se prueba de
  forma determinista.
- `full_fsync` es opt-in: la configuración por defecto en macOS mantiene el
  rendimiento medido y no sobrevive a un corte eléctrico en modo `Safe`, tal
  como documenta ahora el README.

## Verificación

- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`:
  limpios.
- `cargo test --workspace --no-fail-fast`: 48 binarios de prueba, **421
  pruebas pasan, 0 fallan** (incluye las 13 nuevas de esta revisión y las
  pruebas doc).
- Python: `python3 -m unittest discover -s bindings/python/tests` (12 pruebas,
  incluye las cuatro nuevas de ids de petición y clasificación de errores).
- Node: `node bindings/node/test.js` y `node bindings/node/test-integration.js`
  contra el sidecar real.

## Rendimiento

Medición con `examples/audit_performance.rs` (mismo arnés del informe del
11 de septiembre): base `3f7267b` compilada desde un worktree limpio frente
al árbol actual, ambos en `--release`, alternando el orden, en el mismo equipo
(Apple M5, macOS, sin otras cargas). Durabilidad `Fast`, 200 consultas por
fase. Cifras: mediana del p50 de cada repetición, en µs (la carga en s);
positivo significa más lento.

| Fase | Base | Actual | Variación | Rango base | Rango actual |
|---|---:|---:|---:|---:|---:|
| 20K filas, 8 repeticiones | | | | | |
| carga 20K filas (s) | 0,013 | 0,013 | −0,1% | 0,010–0,017 | 0,012–0,014 |
| `cursor_id` | 4,44 | 4,46 | +0,5% | 4,29–4,67 | 4,17–4,50 |
| `cursor_index` | 6,85 | 7,04 | +2,7% | 6,71–7,04 | 6,38–7,08 |
| `cursor_scan` | 5 347 | 5 227 | −2,2% | 5 170–5 518 | 5 135–5 381 |
| `and_index_last` | 4,90 | 4,73 | −3,4% | 4,54–5,17 | 4,58–5,21 |
| `and_index_first` | 4,33 | 4,38 | +0,9% | 4,25–4,50 | 4,21–4,54 |
| `top_k` | 5 627 | 5 649 | +0,4% | 5 501–5 685 | 5 486–5 728 |
| `full_sort` | 9 581 | 9 535 | −0,5% | 9 426–9 696 | 9 463–9 791 |
| `commit_plain` | 3,02 | 3,10 | +2,7% | 2,96–4,50 | 3,00–3,25 |
| `commit_identity` | 4,15 | 4,23 | +2,0% | 4,00–7,67 | 4,13–4,38 |
| `commit_foreign_key` | 6,54 | 6,60 | +1,0% | 6,33–6,67 | 6,13–6,75 |
| 100K filas, 3 repeticiones | | | | | |
| carga 100K filas (s) | 0,048 | 0,050 | +4,3% | | |
| `cursor_id` | 4,21 | 4,17 | −1,0% | | |
| `cursor_index` | 6,38 | 6,42 | +0,7% | | |
| `cursor_scan` | 26 447 | 25 853 | −2,2% | | |
| `top_k` | 28 076 | 28 046 | −0,1% | | |
| `full_sort` | 48 349 | 48 211 | −0,3% | | |
| `commit_plain` | 3,13 | 3,21 | +2,7% | | |
| `commit_identity` | 4,33 | 4,38 | +0,9% | | |
| `commit_foreign_key` | 6,54 | 6,79 | +3,8% | | |

Todas las variaciones caen dentro del rango entre repeticiones de la propia
base (los rangos de base y actual se solapan en cada fase). Las diferencias de
commit son de decenas de nanosegundos sobre 3–7 µs y coinciden con el coste
esperado de las comprobaciones añadidas (canonicalización de `-0.0`, rechazo
de NaN, búsqueda del índice único que antes también se hacía). El arnés no
ejercita `Safe` (dominado por `fsync`; los cambios ahí son sólo de ruta de
error) ni `Balanced` (el temporizador sólo sincroniza cuando ningún commit lo
ha hecho en el intervalo, por lo que bajo carga sostenida no añade fsyncs).
El coste nuevo real es en apertura: una pasada CRC sobre los índices
derivados, no medida por este arnés.

`cargo run --release --example stress -- --smoke` pasa contra el modelo en
memoria con la construcción actual.
