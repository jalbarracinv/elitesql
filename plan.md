# ClawDB Plan de Desarrollo

Plan de ejecucion derivado de [specs.md](specs.md). Las fases siguen el roadmap de la spec, con criterios de aceptacion, estrategia de testing y decisiones de arquitectura ya cerradas.

## Decisiones cerradas

Estas preguntas quedaron abiertas en la spec y ya estan resueltas:

1. **Concurrencia: un solo proceso, multiples threads.** Lock file al abrir la DB para impedir que un segundo proceso la abra. Cubre servidores web desplegados como un solo proceso con concurrencia interna (Node, Go, Rust, Java, Python con threads o async): mientras un request escribe, los demas leen y escriben en paralelo via MVCC. Despliegues multi-worker (gunicorn con varios workers, PHP-FPM) no abren el archivo directamente: usan el modo sidecar `clawdb serve` (Phase 4), un proceso unico con el motor embebido que expone la misma API por Unix socket. Alternativa sin sidecar: `gunicorn -w 1 --threads N` o uvicorn single-worker async. Multi-proceso real (varios procesos abriendo el mismo archivo) queda fuera de V1; el diseno del manifest no debe impedirlo a futuro, pero no se paga su costo ahora.
2. **Queries: dialecto SQL minimo desde Phase 2.** Parser y ejecutor de un subset estricto (SELECT/INSERT/UPDATE/DELETE, WHERE simple, ORDER BY, LIMIT, INNER/LEFT/RIGHT JOIN). El schema se declara via DDL (`CREATE TABLE`) con los tipos V1, lo que resuelve tambien el formato de declaracion de esquemas.
3. **ANN: crate existente detras de interfaz propia.** HNSW via crate maduro (`hnsw_rs`, `usearch` o equivalente, a evaluar en Phase 3) encapsulado en un trait `AnnIndex` propio. La persistencia y el rebuild son formato propio, de modo que reemplazar el crate despues no cambia el formato en disco.
4. **Primary key: ULID text autogenerado.** Todo registro recibe un id `text` tipo ULID generado por el motor si no se provee. Coincide con la API C (`const char *id`), es ordenable por tiempo y amigable para sync futuro.

## Estructura del workspace

Cargo workspace con crates separadas por responsabilidad:

```text
clawdb/
  crates/
    clawdb-core/      # storage, MVCC, WAL, manifest, recovery, indices, compaction
    clawdb-sql/       # parser, planner heuristico, ejecutor del dialecto
    clawdb-vector/    # trait AnnIndex, integracion HNSW, persistencia y rebuild
    clawdb-ffi/       # C ABI estable (cdylib + header generado con cbindgen)
    clawdb-cli/       # binario clawdb: check, repair, import/export, repl
  bindings/
    python/           # sobre la C ABI
    node/             # sobre la C ABI
  tests/
    crash/            # harness de crash injection
    fuzz/             # cargo-fuzz targets: WAL, manifest, parser
  benches/            # criterion + comparativas vs SQLite
```

Infraestructura desde el dia uno:

- CI: build + tests + clippy + fmt en cada PR; fuzzing corto nightly.
- `format_version` en el manifest desde el primer byte escrito a disco.
- Golden files del formato en disco para detectar roturas de compatibilidad.

## Fases

Las estimaciones asumen un desarrollador a tiempo completo y son orientativas.

### Phase 0: Prototype (~3-4 semanas)

Objetivo: validar el nucleo append-only y la ergonomia de la API antes de invertir en durabilidad.

Tareas:

- Crate `clawdb-core` con formato append-only simple (carpeta autocontenida).
- Tipos V1: `bool`, `int64`, `float64`, `text`, `blob`, `timestamp`, `json` (vector llega en Phase 3).
- Insert/get/update/delete con versionado por registro y tombstones.
- Generacion de ULID como PK por defecto.
- Snapshot por version (referencia logica, sin GC todavia).
- Indice primario en memoria reconstruido al abrir.
- Harness de benchmarks (criterion) contra SQLite.

Criterios de aceptacion:

- Lecturas por id dentro de 2x SQLite; inserts secuenciales iguales o mejores que SQLite en el mismo hardware.
- Benchmarks reproducibles con un solo comando.
- Explicitamente NO hay garantias de crash-safety en esta fase.

Estado (2026-08-06): implementada en `crates/clawdb-core`. Tests en verde (12). Benchmarks en Apple Silicon: reads por id 1.39us vs 2.87us de SQLite (2x mas rapido); inserts secuenciales con commit por operacion 248K/s vs 59K/s de SQLite autocommit (4.2x); SQLite en transaccion unica (811K/s) sigue ganando al modo por-operacion, lo esperado hasta batch commits en Phase 1. Extra ya cubierto: CRC32 por entrada y descarte de torn tail al abrir.

### Phase 1: MVP Storage (~8-10 semanas)

Objetivo: las garantias fail-safe de la spec, completas y demostrables.

Tareas:

- WAL durable con checksum por entrada y replay idempotente.
- Manifest atomico + `manifest.prev`, publish via rename atomico.
- Lock file para exclusion de proceso.
- MVCC real: cada transaccion lee desde snapshot estable.
- Commits optimistas con validacion de conflictos (`CONFLICT_RETRY`).
- Secondary indexes y unique indexes con validacion en commit.
- Crash recovery al abrir segun la secuencia de la spec (manifest -> manifest.prev -> WAL replay -> rebuild de indices dirty).
- Checksums en manifest, WAL, bloques de segmento.
- Modos de durabilidad `safe`, `balanced`, `fast`.
- Compaction inicial en background que respeta snapshots referenciados.
- `clawdb_check` basico (validar checksums y referencias).
- Catalogo de errores del motor (los `clawdb_status` que expondra la FFI).

Criterios de aceptacion:

- Suite de crash injection (ver Testing) pasa miles de iteraciones: tras kill en cualquier punto, la DB abre al ultimo estado commiteado completo.
- Un commit nunca es visible parcialmente, verificado por property tests.
- Compaction nunca elimina datos referenciados por un snapshot vivo (test dedicado).
- Fuzzing de WAL y manifest sin panics ni estados corruptos aceptados.

Estado (2026-08-06): completa en `crates/clawdb-core`. Implementado: WAL durable con CRC y replay idempotente; manifest atomico + manifest.prev con heal seguro; lock file (flock); MVCC con transacciones (begin/commit/rollback), snapshots como guards que la compaction respeta, y commits optimistas con `Error::Conflict` (CONFLICT_RETRY); indices secundarios y unicos validados en commit; modos safe/balanced/fast; checkpoint memtable->segmentos con rotacion de WAL; compaction stop-world; `check()`; catalogo de errores con codigos para la futura FFI. Verificacion: 41 tests incluyendo crash injection con kill -9 real (120 rondas corridas en CI local), fuzzing de corrupcion (1000 seeds, sin panics), test de modelo aleatorio con snapshots/reopen/compaction, y atomicidad de commits multi-registro ante WAL cortado. Benchmarks (Fast vs SQLite sync=OFF): reads 0.62us vs 2.72us (4.4x); inserts por-operacion 187K/s vs 63K/s (3.0x); batch txn 311K/s vs 841K/s de SQLite (pendiente de optimizar el staging path). Pendiente de la fase: compaction en background automatica (hoy es explicita via `compact()`).

### Phase 2: Query Layer (~6-8 semanas)

Objetivo: el dialecto SQL minimo, deliberadamente limitado.

Tareas:

- Parser del subset: evaluar `sqlparser-rs` restringido vs parser propio pequeno; decidir en la primera semana de la fase con un spike de 2-3 dias.
- DDL: `CREATE TABLE` con tipos V1, indices normales y unique.
- DML: `SELECT`, `INSERT`, `UPDATE`, `DELETE`.
- `WHERE` con filtros simples sobre campos indexables y no indexables.
- `ORDER BY` basico y `LIMIT`.
- `INNER JOIN`, `LEFT JOIN`; `RIGHT JOIN` normalizado internamente como `LEFT JOIN` invertido.
- Planner heuristico minimo: usar indice cuando existe, si no full scan; filtros empujados antes del join cuando sea posible.
- Rechazo explicito y con mensaje claro de todo lo fuera del subset (FULL OUTER, subqueries, CTEs).

Criterios de aceptacion:

- Suite estilo sqllogictest cubriendo el subset completo, incluyendo casos que deben fallar con error claro.
- Joins con indice sobre datasets de 1M registros con latencia razonable documentada en benchmarks.
- El parser fuzzeado no produce panics.

Estado (2026-08-07): completa como modulo `sql` dentro de `clawdb-core` (la crate separada `clawdb-sql` del workspace se pospone hasta que la FFI lo pida). Decision del spike: parser propio recursive descent, no sqlparser-rs — subset chico y cerrado, mensajes de error propios ("not supported in V1" con alternativa), cero dependencias nuevas, y guard de profundidad de expresiones para que el fuzzing no pueda reventar el stack. Implementado: CREATE TABLE (tipos V1 estrictos con sugerencias), CREATE [UNIQUE] INDEX, INSERT multi-fila atomico que devuelve ids, SELECT con WHERE (=, <>, <, <=, >, >=, AND/OR/NOT, IS NULL, IN), ORDER BY multi-columna, LIMIT/OFFSET, alias, INNER/LEFT/RIGHT JOIN encadenados (RIGHT = LEFT invertido), UPDATE/DELETE con conteo de afectadas y retry ante conflicto. Planner heuristico: point lookup por id, igualdad indexada via find_eq, pushdown de filtros pre-join, index nested-loop cuando el probe es chico y la columna de join esta indexada, hash join como fallback. Consistencia: SELECT es read-committed; snapshots consistentes via API Rust. Verificacion: 19 tests sqllogictest-style + 4 de fuzzing (6000 inputs aleatorios/mutados sin panics, nesting profundo cortado por guard). Benchmarks sobre 1M ordenes + 10K usuarios: point lookup via indice unico 3.9us (SQL completo), join indexado con ORDER BY+LIMIT 361us, full scan sin indice 1.1s (limite conocido de la spec). Documentacion de usuario en manual.md.

### Phase 2.5: Agregados basicos y tipos date/time (~2-3 semanas)

Agregada el 2026-08-07: los agregados se referenciaban en manual.md como "Phase 2.x" sin fase real asignada, y date/time se promueven de opcionales-V1.1 a necesarios por decision de producto.

Tareas:

- Tipo `date`: dias desde epoch Unix (i32 logico). Literal SQL 'YYYY-MM-DD' coercionado por tipo de columna; validacion de fecha real.
- Tipo `time`: microsegundos desde medianoche (i64). Literal SQL 'HH:MM:SS[.ffffff]'; validacion de rango.
- Ambos comparables, ordenables e indexables como cualquier escalar.
- Agregados globales: COUNT(*), COUNT(col), SUM, AVG, MIN, MAX.
- GROUP BY por una o varias columnas (hash aggregation en el executor).
- HAVING con predicados simples sobre los agregados del SELECT.
- Composicion con el subset existente: WHERE antes de agrupar, ORDER BY sobre columnas de salida, LIMIT; rechazo claro de agregados anidados, DISTINCT dentro de agregados y expresiones.

Criterios de aceptacion:

- Suite sqllogictest-style para agregados incluyendo semantica de NULL (COUNT(col) ignora NULL, SUM/AVG de conjunto vacio es NULL, MIN/MAX ignoran NULL).
- Roundtrip completo de date/time: API Rust + SQL + indices secundarios + ORDER BY; literales invalidos ('2026-02-30', '25:00:00') rechazados con error claro.
- Fuzzing del parser extendido a las nuevas producciones sin panics.

Estado (2026-08-07): completa. Tipos: `date` (dias desde epoch, algoritmo de calendario civil propio con validacion real incluyendo bisiestos — 2100-02-29 se rechaza) y `time` (microsegundos desde medianoche con fraccion opcional .ffffff); literales string coercionados por tipo de columna en INSERT/UPDATE/WHERE (tambien contra indices), constructores publicos Value::parse_date/parse_time/date_from_ymd/time_from_hms_micro; comparables, ordenables, indexables y agrupables. Agregados: COUNT(*)/COUNT(col)/SUM/AVG/MIN/MAX con hash aggregation en orden de primera aparicion, GROUP BY multi-columna (NULLs agrupan juntos), HAVING sobre columnas agrupadas y agregados (incluso agregados que no estan en el SELECT), ORDER BY por nombre de salida o alias, y composicion con WHERE/joins/LIMIT/OFFSET. Semantica NULL estandar; SUM int64 con deteccion de overflow (error, no wrap) y promocion a float64 en mezcla. Rechazos claros: agregados en WHERE (apunta a HAVING), columna no agrupada en SELECT, COUNT(DISTINCT), SUM(*), SUM/AVG sobre columnas no numericas, ORDER BY con llamada a agregado (pide alias). Verificacion: 11 tests de agregados + 8 de date/time/timestamp + fuzz extendido con las nuevas producciones; 99 tests en total, clippy limpio.

Adenda datetime (2026-08-07): en lugar de un tipo `datetime` separado (timestamp ya es el tipo de instante; dos tipos de instante confunden), `timestamp` acepta literales string 'YYYY-MM-DD HH:MM:SS[.ffffff]' interpretados como UTC naive — con separador espacio o T, sufijo Z opcional y fecha sola como medianoche — en INSERT/UPDATE/WHERE y contra indices, con Value::parse_timestamp en la API. Offsets de timezone rechazados a proposito: el motor guarda instantes, la zona horaria es presentacion de la aplicacion.

### Phase 3: Vector Native (~5-6 semanas)

Objetivo: vectores como tipo e indice de primera clase.

Tareas:

- Tipo `vector<float32, N>` con validacion de dimension en insert/update.
- Trait `AnnIndex` propio; spike de 1 semana evaluando `hnsw_rs` vs `usearch` (calidad de recall, memoria, licencia, mantenimiento) y decision documentada.
- Persistencia propia del indice: metadata con checksum, grafo reconstruible desde datos canonicos si se marca corrupto.
- `search_vector` con `top_k`, `metric` (cosine, dot, l2), `ef_search` y filtro por metadata.
- Modos de indexacion `sync` (searchable al commit) y `async` (entra en background).
- Manejo de tombstones/updates en el indice ANN (borrado logico + rebuild en compaction).

Criterios de aceptacion:

- Benchmarks de recall@k contra ground truth en datasets publicos (por ejemplo SIFT1M o subset).
- Rebuild completo del indice desde segmentos canonicos funciona y esta cubierto por tests.
- Crash durante indexacion async nunca corrompe datos canonicos (crash injection extendida a esta fase).

Estado (2026-08-07): completa. Resultado del spike (giro documentado): se evaluo hnsw_rs 0.3.4 y fallo el criterio de recall — su descenso por capas superiores hace un solo paso greedy por capa en vez de iterar al minimo local (search_filter en su hnsw.rs), midiendo recall@10 de 0.47-0.77 con ef=128 y no-monotonico en ef; usearch se descarto por su toolchain C++ (compromete el target WASM). Decision final: HNSW propio (~300 lineas, Malkov & Yashunin con heuristica de seleccion de vecinos), que ademas cumple la vision de largo plazo (control total para persistencia futura e integracion MVCC). Implementado: tipo vector<float32,N> (Value::Vector, Column::vector(dim), validacion de dimension en insert/update, vector(N) en CREATE TABLE y literal JSON-array en INSERT SQL); create_vector_index con metricas cosine/dot/l2 y parametros m/ef_construction; search_vector con top_k, ef_search, filtro de igualdad por metadata y over-fetch escalonado; modos sync (searchable al commit) y async (thread en background con backlog observable y wait_vector_indexing); borrado logico con tombstones filtrados en busqueda y rebuild fisico en compaction; indice reconstruido desde datos canonicos en open (grafo desechable por diseno). Verificacion: 12 tests vectoriales (recall>=0.9 vs fuerza bruta en 2K vectores, filtros, update/delete, reopen, compaction, metricas, async) + crash injection especifico (kill -9 durante indexacion async: datos canonicos y payloads vectoriales intactos, indice rebuildeado usable). Benchmark 100K vectores dim 64 clusterizados con ground truth por fuerza bruta in-process: recall@10 0.88 (ef=128) y 0.97 (ef=256), busqueda 95us (ef=64) a 156us (ef=128). Nota: SIFT1M real queda pendiente de una corrida con el dataset descargado; el harness ya mide recall contra ground truth exacto.

Persistencia del grafo (2026-08-07): implementada. El grafo HNSW completo se serializa en vectors/ (magic CLAWVIDX, CRC32 del cuerpo, identidad del indice y version de commit del dump) al cerrar la DB y al compactar, con escritura atomica (tmp + rename + fsync). Al abrir: si el dump valida y su version es <= la version commiteada, se carga y se aplica catch-up incremental con todo lo commiteado despues del dump (registros cambiados/borrados y dumps que la compaction dejo atras); si esta corrupto, la definicion cambio, o el dump viene "del futuro" (rollback via manifest.prev), se reconstruye desde datos canonicos sin error — el grafo sigue siendo desechable. La carga valida invariantes estructurales (entry point, vecinos dentro de rango y por debajo de su capa) para que un archivo corrupto jamas cause panic. Resultado: abrir 100K vectores pasa de rebuild completo a ~134ms. Tests: carga identica tras cierre limpio, catch-up con dump viejo (inserts/updates/deletes posteriores reflejados), corrupcion con fallback a rebuild, y dump refrescado y encogido tras compaction.

### Phase 4: Developer Experience (~8-10 semanas)

Objetivo: que ClawDB sea usable fuera de Rust.

Tareas:

- `clawdb-ffi`: C ABI estable segun la spec, header generado con cbindgen, versionado semantico de la ABI.
- Binding Python sobre la C ABI, publicado como wheel. Requisito: liberar el GIL durante toda llamada al motor, para que un despliegue single-proceso con threads paralelice de verdad las operaciones de DB.
- Binding Node sobre la C ABI (napi), publicado en npm.
- CLI `clawdb`: repl de queries, `check`, `repair`, import/export (JSON y CSV).
- Modo sidecar `clawdb serve <db>`: un proceso unico con el motor embebido expone la API completa por Unix socket, para despliegues multi-worker (gunicorn, PHP-FPM). Los bindings se conectan de forma transparente (deteccion de socket o connection string) sin cambiar la API del usuario. El motor no cambia: se preservan las garantias single-proceso y los clientes concurrentes aprovechan los escritores paralelos internos.
- `clawdb_repair`: recuperar registros validos, reportar siempre lo descartado, nunca reparar en silencio.
- Modo read-only automatico ante corrupcion no recuperable.
- Docs: getting started, referencia del dialecto SQL, guia de recovery, formato en disco.
- Benchmarks reproducibles publicados con el repo.

Criterios de aceptacion:

- Un usuario puede hacer el flujo completo (create, insert, query, search_vector, snapshot) desde Python y Node sin tocar Rust.
- Demo reproducible: gunicorn con 4 workers contra `clawdb serve`, visitantes concurrentes leyendo y escribiendo sin bloquearse entre si.
- `clawdb check` detecta las corrupciones inyectadas por la suite de crash tests.
- Docs suficientes para onboarding sin leer el codigo.

Estado (2026-08-07): nucleo de la fase completo. Entregado:
- C ABI (`crates/clawdb-ffi`, libclawdb cdylib+staticlib, header include/clawdb.h): open/close/query(SQL)/search_vector/create_vector_index/checkpoint/compact/check/repair. Codigos de estado estables (Error::code), mensajes via clawdb_last_error thread-local, panic-safe (catch_unwind, nunca unwinding a traves de la ABI). Payloads JSON con valores etiquetados ({"$t": date|time|timestamp|blob|json|vector}) via el modulo compartido clawdb_core::jsonio.
- CLI `clawdb` (crates/clawdb-cli): query, repl, tables, check, compact, repair, export/import JSONL con coercion por tipo de columna, serve, version. Sin dependencias de parsing de argumentos.
- `clawdb repair`: salvage a una DB nueva desde segmentos+WAL, tolerante a corrupcion (prefijo valido por archivo), nunca silencioso — reporta recuperados/borrados/saltados con una nota por cada dano.
- Sidecar `clawdb serve <db> <socket>`: Unix socket, protocolo JSON por linea (query, search_vector, create_vector_index, checkpoint, compact, ping), un thread por conexion sobre un Db compartido.
- Binding Python (bindings/python/clawdb.py): ClawDB embebido sobre la C ABI via ctypes — el GIL se libera automaticamente en cada llamada foranea, cumpliendo el requisito de la fase — mas SidecarClient para multi-worker; decodifica date/time/timestamp a datetime.*, blob a bytes, y expone check()/repair().
- Binding Node (bindings/node/clawdb.js): cliente sidecar puro JS sin dependencias, API de promesas, decodifica a Date/Buffer.
- Demo de aceptacion cumplida: examples/gunicorn_demo/run_demo.sh — gunicorn real con 4 workers contra clawdb serve; 8 visitantes x 25 requests concurrentes: 200/200 escrituras registradas, 4 PIDs de worker distintos, wall 0.7s, peor request 41ms, cero bloqueos.
- Verificacion: 105 tests Rust + e2e del CLI completo + smoke FFI Python (flujo con vectores y filtros) + e2e Node sidecar.

Pendiente de la fase (siguiente iteracion): empaquetado y publicacion (wheel PyPI, npm), binding Node nativo napi (hoy Node va via sidecar), snapshots en la ABI (lecturas de bindings son read-committed; snapshots consistentes solo en API Rust), modo read-only automatico ante corrupcion no recuperable, y docs de onboarding dedicadas mas alla de README + manual.md.

### Phase 5: Advanced (sin estimar, priorizar al llegar)

- Full-text basico y `search_hybrid`.
- Blob chunking con checksums por chunk.
- Vectores cuantizados (`vector<int8, N>`, IVF/PQ).
- Target WASM.
- Sync local-first opcional.
- Explorar modo single-file.

## Estrategia de testing (transversal)

El testing de durabilidad es trabajo de primera clase, no un afterthought. Se construye en Phase 1 y se mantiene en todas las fases:

- **Crash injection**: harness que ejecuta workloads matando el proceso en puntos aleatorios (incluyendo entre write y fsync, y a mitad del rename del manifest), luego reabre y verifica invariantes. Corre en CI con budget corto y nightly con budget largo.
- **Property tests** (proptest): invariantes de MVCC (un reader nunca ve estado intermedio), idempotencia del WAL replay, equivalencia semantica update-then-read.
- **Fuzzing** (cargo-fuzz): WAL corrupto, manifest corrupto, statements SQL malformados. Nunca panic, nunca aceptar estado invalido.
- **Tests de concurrencia**: muchos readers + multiples writers concurrentes con verificacion de serialidad de commits.
- **Golden files**: fixtures binarias del formato en disco por version; cambiar el formato exige bump explicito de `format_version`.

## Riesgos principales

- **SQL desde el inicio agranda Phase 2.** Mitigacion: subset estricto e inamovible durante V1, spike temprano de sqlparser-rs para no escribir parser desde cero si no hace falta.
- **Integracion de crate HNSW con MVCC/tombstones.** Los crates ANN no suelen soportar borrado bien. Mitigacion: trait propio, borrado logico y rebuild en compaction; el formato en disco es nuestro.
- **mmap + crash safety** tiene esquinas oscuras (paginas sucias, msync). Mitigacion: mmap solo para lecturas; todas las escrituras via write + fsync explicito.
- **Alcance V1.** Cada feature nueva debe justificar su peso (principio de la spec); todo lo que no este en el alcance V1 se rechaza por defecto.

## Preguntas abiertas restantes

Decidir a mas tardar al inicio de la fase que las necesita:

- **Nullabilidad** (Phase 0): propuesta — campos nullable por defecto, `NOT NULL` opt-in en el DDL.
- **Semantica exacta de `json`** (Phase 2): si se puede filtrar por paths internos en V1 o solo almacenar/leer.
- **Politica de retencion de snapshots** (Phase 1): cuantos snapshots vivos y como se liberan (API explicita vs TTL).
- **Nombre y formato del lock file** y comportamiento ante procesos zombie (Phase 1).
- **Protocolo del sidecar** (Phase 4): propuesta — protocolo binario minimo versionado sobre Unix socket; evitar dependencias pesadas tipo gRPC.
