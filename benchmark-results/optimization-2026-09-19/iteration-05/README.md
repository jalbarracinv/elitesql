# Iteración 05: actualizaciones delta en lugar de conflictos

Continuación de la [iteración 04](../iteration-04/README.md). La pregunta: por
qué `checkout` y `restock` agotan sus reintentos por conflicto con 100 y 500
usuarios, y si se puede evitar sin perder velocidad.

Fecha: 2026-09-23. Base (`before-elitesql`): el árbol de trabajo de la
iteración 04 sin commitear sobre `92e0290`. Resultado (`after-elitesql`): ese
mismo árbol más este cambio. Hashes en `build-identities.txt`.

## Diagnóstico

Todos los reintentos agotados eran `products/… changed after this transaction
began`, en `checkout` (≈ 90 %) y `restock`, sobre los 20 productos calientes.
El commit valida con la regla de que gana el primero que confirma: si una fila
escrita cambió después del snapshot de `begin()`, se aborta la transacción
entera. Un checkout son de 7 a 2·n+7 sentencias por socket, así que con 500
usuarios dura decenas de ms y casi siempre otro checkout confirma antes sobre
el mismo producto. Pero `UPDATE products SET stock = stock - ?, … WHERE id = ?
AND stock >= ?` no depende del valor que vio la transacción, solo de que al
confirmar la guarda siga cumpliéndose.

## Cambio

Un `UPDATE` cuyos `SET` son todos `col = col + v` o `col = col - v` guarda,
junto a la fila, un paso que sabe reaplicarse: reevalúa el `WHERE` y aplica
los deltas sobre un registro dado (`DeltaUpdate`, `sql/exec.rs`). En la
validación del commit, bajo el mutex de siempre, si la fila cambió después del
snapshot, `rebase_change` (`db/commit.rs`) lee la última versión, reaplica los
pasos en orden de sentencia y sustituye el registro y su payload; la trama del
WAL se vuelve a codificar solo en ese caso. El WAL recibe la fila ya resuelta,
así que el formato en disco y la recuperación no cambian.

El conflicto se mantiene si:

- el `WHERE` falla sobre la versión nueva (se agotó el stock) o la aritmética
  falla ahí (desbordamiento): el reintento ve el resultado real;
- la fila se borró entretanto;
- la transacción devolvió esa fila a quien la llamó, antes o después del
  `UPDATE` (`ObservedRows`, en `db.rs`; por encima de 1 024 filas por tabla, la
  tabla entera cuenta como leída);
- la fila recibió otra escritura en la misma transacción (asignación, `*`,
  `/`, `INSERT`, `DELETE`);
- la tabla tiene columnas blob (reescribir bajo el mutex no debe crear
  ficheros blob).

Solo se admiten sumas y restas porque son conmutativas: el resto de lecturas
de la transacción siguen en su snapshot, y solo un cambio independiente del
orden mantiene la coherencia de "este UPDATE se ejecutó al confirmar". El
camino sin contención no cambia; el contador `delta_rebased_rows` de
`{"op":"stats"}` dice cuántas filas se rescataron.

## Resultado

Sweep encadenado 10 → 100 → 500 usuarios, 20 s por etapa, esquema `baseline`,
dos rondas intercaladas antes / después (`ab/`, resumen en `ab-summary.txt`):

| usuarios | antes ops/s | después ops/s | p99 antes → después | éxito antes → después | reintentos agotados antes → después | reintentos por conflicto antes → después | filas rebasadas |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 16 017 | 16 220 | 5,0 → 5,0 ms | 100 → 100 % | 0 → 0 | 311 → 0 | 463 |
| 100 | 19 423 | 19 681 | 31,5 → 30,4 ms | 99,989 → 100 % | 55 → 0 | 4 149 → 0 | 4 045 |
| 500 | 13 282 | 12 829 | 212 → 212 ms | 99,917 → 99,999 % | 274 → 0 | 5 250 → 0 | 4 687 |

- Los conflictos desaparecen del todo en las dos rondas: cero reintentos y
  cero agotados en todos los niveles.
- El throughput queda igual dentro del ruido de esta máquina (±20 % entre
  corridas del mismo binario): +1,3 % a 10, +1,3 % a 100 y −3,4 % a 500, donde
  las dos rondas del binario nuevo dieron 11 966 y 13 692 ops/s, frente a
  13 371 y 13 192 del anterior.
- En la ronda 1 del binario nuevo, a 500 usuarios, hubo 3 `error:16` de
  `admin_dashboard` (tiempo agotado en la admisión de memoria de consultas).
  Es el problema de admisión que describe la iteración 04, al recorrer
  `orders` ya crecida; no pasa por el camino del commit y no apareció en la
  ronda 2.

## Integridad

- `bash scripts/acceptance.sh` terminó con código 0 (`acceptance.log`): fmt,
  clippy estricto, 56 suites Rust, 12 pruebas Python, Node.
- Pruebas nuevas en `crates/elitesql-core/tests/delta_updates.rs` (10), con
  tres mutaciones comprobadas: sin rebase fallan 5; ignorando las filas
  observadas falla la prueba de lectura; sin reevaluar el `WHERE` fallan 3,
  entre ellas la de stock escaso, que exige vender exactamente lo que hay.
- Todos los sweeps: invariantes de negocio correctos, 0 violaciones de
  read-your-writes y `elitesql check` sin errores, también justo después de
  matar el servidor.

## Reproducción

Copiar `before-elitesql` y `after-elitesql` junto a `run_ab.sh`, ejecutarlo
sin compilaciones ni otros benchmarks en paralelo y resumir con
`python3 summarize.py`.
