# Iteración 04: contención de locks en el sidecar

Continuación de [iteración 02](../iteration-02/README.md) y de los perfiles de
la iteración 03. La pregunta: por qué el sidecar rinde menos que SQLite bajo
concurrencia, y cuánto se puede recuperar sin perder integridad.

Fecha: 2026-09-23. Base: commit `92e0290` (`before-elitesql`); resultado:
árbol de trabajo de esta iteración (`after-final-elitesql`). Hashes en
`build-identities.txt`. Máquina: macOS arm64, 10 núcleos, con carga de fondo
(navegador, Docker; load average 12–15), por eso todas las comparaciones son
intercaladas A/B en la misma sesión.

## Resultado

Sweep encadenado 10 → 100 → 500 usuarios, 20 s por etapa, dos repeticiones
intercaladas antes / después / SQLite (`final/`, resumen en `final-summary.txt`):

| usuarios | antes ops/s | después ops/s | Δ | p50 antes → después | p99 antes → después | éxito antes → después | después / SQLite |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 17 311 | 18 447 | +6,6 % | 0,29 → 0,28 ms | 5,1 → 4,8 ms | 100 → 100 % | 0,67× (antes 0,63×) |
| 100 | 19 041 | 21 195 | +11,3 % | 1,60 → 1,06 ms | 32,1 → 28,1 ms | 99,997 → 99,979 % | 0,78× (antes 0,70×) |
| 500 | 14 485 | 14 896 | +2,8 % | 3,88 → 2,12 ms | 200,9 → 192,7 ms | 99,784 → 99,916 % | 0,60× (antes 0,58×) |

A 500 usuarios los reintentos agotados por conflicto bajan de ~660 a ~280 por
etapa. Los re-chequeos `final-checked-{1,2}` (misma versión, verificador
corregido, ver Integridad) dan 15 188 y 15 419 ops/s a 500.

El coste secuencial embebido no cambia (`ops-*.txt`, dos pares intercalados):
mediana ponderada 67,9 / 67,8 µs antes y 67,5 / 67,4 µs después, 1,94× SQLite
en ambos casos. Los cambios atacan la concurrencia, no el trabajo por fila.

## Diagnóstico

1. **Mutex global de snapshots.** `Db::snapshot()` tomaba el registro de
   snapshots y, con él tomado, esperaba `state.read()`. Cuando un commit tenía
   o esperaba `state.write()`, el primer lector se quedaba con el mutex y todo
   `snapshot()` y todo `Snapshot::drop` hacían cola detrás (46 800 muestras en
   `__psynch_mutexwait` en el perfil de la iteración 03).
2. **Lecturas que se degradaban a recorridos completos bajo el lock.** Con un
   snapshot ya histórico (un commit entre abrirlo y leer):
   `find_eq_batch_version` recorría la tabla entera en una sola retención de
   `state.read()` buscando `token = ?`, `cart_id = ?`…, y el recorrido ordenado
   de `browse` caía a scan + sort. La sonda de locks (`lock-probe3-100.tsv`)
   atribuye a esa búsqueda 6,6 s de espera de escritores en 15 s.
3. **Throttle de lecturas puntuales mal calibrado.** `PointReadAdmission`
   contaba como «escritores» todos los commits en curso, también los ~40 que
   hacían cola por el mutex de commit sin tocar `state`, así que dejaba pasar
   **una** lectura puntual a la vez (135 000 esperas por etapa).
4. **Scans troceados en exceso.** Con un committer esperando, los scans cedían
   el lock cada 64 filas y cada reanudación hacía cola detrás del escritor: un
   `admin_dashboard` sobre la tabla `orders` ya crecida tardaba 1,5 s a 500
   usuarios y agotaba la admisión de memoria (`error:16`).

Lo que queda (sección final) es el pipeline de commit serializado y la
arquitectura sidecar, no los locks de lectura.

## Cambios conservados

Cada uno se midió en A/B intercalado contra el anterior; `exploration/` guarda
los runs.

| # | Cambio | Dónde | Efecto medido |
|---|---|---|---|
| P1 | `snapshot()` registra la versión desde un atómico (`published_version`) sin `state`; registra, relee y reintenta si avanzó. El registro pasa a lock hoja (orden `state → snapshots`); compactación y estimación de deuda ya no lo retienen durante su recorrido | `db.rs` | esperas de mutex 46,8k → 3k muestras; p50 −23 % a 100 frente a la base |
| P2 | Caché de esquemas publicada por `set_catalog` (`published_schemas`); `expand_delete_cascades` ya no clona el catálogo si no hay borrados | `db.rs`, `maintenance.rs` | quita ~545k adquisiciones de `state` |
| P3 | El recorrido ordenado de un SELECT autocommit registra su snapshot bajo el mismo guard con el que lee; hasta 3 intentos antes del scan + sort | `sql/exec.rs`, `db.rs` | `browse` 1,4 ms de media (5,8 ms sin este cambio) |
| P4 | `find_eq_batch_version` con snapshot histórico usa índice + ids cambiados desde la versión (`change_log`), como `Txn::find_eq`, en vez del recorrido completo | `db.rs` | culpa de espera de escritores 6,6 s → 0,9 s; a 100, +6 % y p50 −36 % frente a la base |
| P5 | La búsqueda vectorial decodifica los candidatos fuera del lock | `db.rs` | colas: p99 de `recommend` 19 → 12 ms |
| P6 | El throttle de lecturas puntuales usa los committers que esperan realmente `state.write()` (`state_write_waiting`) | `db.rs`, `commit.rs` | +5 %, p50 1,12 → 0,86 ms |
| P11 | `Txn::schema` lee la caché publicada; la marca de id alto se lee al primer INSERT (más tarde ⇒ nunca menor ⇒ seguro) | `db.rs` | neutro-positivo (+1,4 %) |
| P12 | El sidecar solo llama `setsockopt` cuando cambia el timeout deseado (antes 4 llamadas por petición) | `serve.rs` | +1,4 %, menos CPU del servidor |
| — | Trozo de scan único de 1 024 filas (antes 256, o 64 con un committer en cola) | `reads.rs` | sweeps encadenados a 500: con trozos de 64, 12,8k ops/s y p99 del dashboard 5,7 s; con 1 024, 15,2k y 0,86 s (cada run con la base intercalada); 4 096 perdía a 100 |

## Experimentos descartados (medidos)

| Experimento | Resultado |
|---|---|
| `parking_lot::RwLock` para `state` | −7 % y p50 peor, dos veces |
| Trozo de scan de 16 filas | sin efecto |
| Límite `--max-statements` 8 / 10 / 16 / 24 | neutro o peor |
| Handoff no justo del mutex de commit | sin efecto |
| Particionar lotes del coordinador (apartar solo al miembro en conflicto) | sin efecto: las caídas eran lotes de un solo miembro |
| Tablas con índice de texto/vector dentro del coordinador | −11 %, p99 ×1,7 (confirma la medición original documentada en `commit.rs`) |
| Compuerta que da prioridad al líder del coordinador | −28 %: inanición de los checkouts |
| Lectores que giran con `try_read` antes de dormir | −3 %, espera del escritor 34 → 51 µs |
| Ceder el lock del scan cada 8 filas ante un committer | neutro a 100, dashboard ×10 a 500 |
| Admisión de memoria FIFO con reserva para el primero en cola | quita `error:16` pero 9,2k ops/s a 500 |
| Cota superior: no ejecutar búsquedas de texto y vector | +9 % solo por trabajo evitado; espera por `state` sin cambio ⇒ no se separaron sus índices |

## Integridad

- `bash scripts/acceptance.sh` terminó con código 0 (`acceptance.log`): fmt,
  clippy estricto, 478 ejecuciones Rust correctas, 12 pruebas Python sin
  fallos (con el `ResourceWarning` de socket ya presente en la iteración 02),
  Node.
- Pruebas nuevas, validadas contra mutaciones (fallan con el error inyectado):
  - `tests/snapshot_registry.rs`: transferencias concurrentes con checkpoints y
    compactaciones; cada lector mantiene su snapshot hasta que termina una
    compactación completa y verifica total y filas. Falla si `snapshot()` no
    registra.
  - `historical_equality_tests` (`db.rs`): lookup por igualdad paginado a una
    versión histórica tras actualizar, mover, borrar e insertar filas a ambos
    lados de un checkpoint. Falla si se omiten los ids del `change_log`.
- Todos los sweeps: invariantes de negocio correctos y `elitesql check` sin
  errores, también justo después de `kill -9` del servidor.
- **Falso positivo del simulador, corregido.** Los runs posteriores a P6
  contaban a veces «violaciones de consistencia» (29 en `final/after-2`). Un
  run instrumentado que reprodujo el caso (11 violaciones, binario P6,
  `violations.jsonl`) muestra que todas eran `view_cart` de un usuario con más
  de 100 productos en el carrito: la consulta tiene
  `LIMIT 100` y el verificador exigía igualdad con todo el carrito; ninguna
  cantidad difería. Con el motor más rápido ese usuario llega a 100 artículos
  dentro de la ventana. `vuser.py` ahora comprueba una vista truncada fila a
  fila (producto y cantidad) y deja de dar el carrito por conocido; la
  consulta no cambió. Con el verificador corregido: 0 violaciones en
  `final-checked-{1,2}`.

## Por qué seguimos por detrás de SQLite

- **Arquitectura de la comparación.** SQLite corre dentro de cada proceso del
  generador; el sidecar hace un viaje por socket y JSON por sentencia. El
  servidor gasta ~250 µs de CPU por operación (≈6 núcleos para 23k ops/s a
  100), frente a ~93 µs por operación de SQLite con Python incluido. La
  máquina queda limitada por CPU.
- **Commit serializado.** El líder del coordinador está ocupado ~91 % del
  tiempo. Un commit individual retiene el mutex 323 µs: 79 µs de `write()` del
  WAL y ~200 µs de apply, que incluyen esperar `state.write()` y, al
  soltarlo, despertar uno a uno a los lectores dormidos (medido: 3 µs por
  lector; 96 lectores = 297 µs).
- **Lo que haría falta.** Publicar el estado sin bloquear lectores (RCU o
  locks por tabla del índice primario), sacar el `write()` del WAL de la
  sección serial sin romper «en el WAL antes de ser visible», y un transporte
  en proceso para comparar sin IPC. Son cambios de arquitectura, fuera de esta
  iteración.

## Reproducción

Desde la raíz, sin compilaciones ni otros benchmarks en paralelo:

```bash
cargo build --release --workspace --locked
python3 examples/saas_simulation/sweep.py --transport sidecar \
  --elitesql-bin "$PWD/target/release/elitesql" --levels 10,100,500 \
  --duration 20 --warmup 3 --ramp 3 --products 5000 --accounts 20000 \
  --scenario compound-index --out DIRECTORIO
python3 examples/saas_simulation/sweep.py --transport sqlite --levels 10,100,500 \
  --duration 20 --warmup 3 --ramp 3 --products 5000 --accounts 20000 \
  --scenario compound-index --out DIRECTORIO_SQLITE
ELITESQL_LIB=DIR_CON_LIBELITESQL python3 examples/saas_simulation/ops_cost.py \
  --products 5000 --users 20000 --iterations 60 --heavy-iterations 20 \
  --repetitions 3 --scenario compound-index --out RESULTADO.json
```

Para comparar dos versiones, copiar cada binario y alternar las ejecuciones
(A, B, A, B); en esta máquina el mismo binario varía hasta un 20 % entre
horas. Los binarios y bibliotecas locales, y los `*.sample.txt`, son
artefactos de medición y no se publican (`.gitignore`).
