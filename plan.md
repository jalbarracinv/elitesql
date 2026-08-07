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
