# Auditoría técnica y plan de mejora de EliteSQL

Fecha: 2026-09-10. Revisión: `d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c`.

**Ampliación de integridad:** la [segunda revisión](revision-integridad-2026-09-10.md)
reproduce fallos adicionales de restricciones, atomicidad, índices y WAL. Su
orden de corrección prevalece sobre el calendario de optimización de este
documento; las estimaciones siguientes cubren únicamente el alcance inicial.

El motor tiene una base valiosa: WAL con recuperación, MVCC, índices persistidos,
compactación en segundo plano, ejecución con archivos temporales y pruebas de
fallos reales. La prioridad debe ser corregir inconsistencias de resultados y
cerrar huecos en el control de recursos antes de optimizar más el almacenamiento.
Las mayores oportunidades inmediatas de rendimiento están en la ruta SQL del
sidecar, la elección del índice y la combinación de archivos de ordenación.

Esta revisión comprende core, SQL, CLI/sidecar, FFI, clientes Python/Node,
pruebas y resultados de benchmarks versionados. No modifica la implementación.
Los casos reproducidos utilizaron bases temporales; las cifras de rendimiento
históricas no son mediciones nuevas de esta auditoría. No constituye una
verificación exhaustiva de todos los interleavings de concurrencia ni una
auditoría de vulnerabilidades de dependencias.

Prioridades: **P0** bloquea una entrega que prometa resultados fiables;
**P1** afecta disponibilidad, escalabilidad o el uso habitual;
**P2** mejora rendimiento o mantenibilidad y requiere medir su beneficio.

## Hallazgos y acciones

### H01 · P0 · Comparaciones SQL con pérdida de precisión — reproducido

En `crates/elitesql-core/src/sql/exec.rs:989`, `numeric()` convierte `Int64` y
`Timestamp` a `f64`. `cmp_vals()` y `sort_cmp()` usan esa conversión incluso
entre dos enteros. Valores distintos alrededor de `2^53` terminan comparándose
como iguales. También afecta `MIN`/`MAX` y predicados residuales.

Caso observado, insertando primero `9007199254740992` y luego
`9007199254740993`, con ids físicos `a` y `b`:

| Consulta | Resultado observado | Resultado correcto |
|---|---|---|
| `WHERE n = 9007199254740993 OR n = 0` | Ambos registros | Sólo el segundo |
| `ORDER BY n DESC` | Menor, mayor | Mayor, menor |
| `SELECT MAX(n)` | `9007199254740992` | `9007199254740993` |

La igualdad simple desde CLI devuelve una fila porque su acceso por igualdad
filtra antes. La misma igualdad desde el sidecar devuelve dos. Es una diferencia
observable entre rutas públicas, no sólo una precisión teórica.

**Acción:** comparar enteros sin conversión y definir comparaciones mixtas
entero/float que conserven orden y exactitud. Unificar igualdad, ordenación,
agregados, filtros y claves de índices/joins según ese contrato.

**Aceptación:** casos alrededor de `2^53`, extremos de `i64`, valores negativos,
ceros flotantes y timestamps; resultados equivalentes con/sin índices, con/sin
spill y entre CLI, Rust, FFI y sidecar. Añadir pruebas diferenciales para el
subconjunto SQL compartido con SQLite, explicitando diferencias de dialecto.

### H02 · P0 · Un índice cambia el resultado de un JOIN — reproducido

`join_key()` en `sql/exec.rs:1508` normaliza timestamps, pero mantiene diferentes
codificaciones para `Int64(1)` y `Float64(1.0)`. El hash join y el join mediante
índice no aplican el mismo contrato de igualdad.

Con una tabla `ints(n int)` que contiene `1` y otra `floats(n float64)` que
contiene `1.0`, `SELECT ints.n FROM ints JOIN floats ON ints.n = floats.n`
devuelve cero filas. Tras `CREATE INDEX ON floats (n)`, devuelve una fila.

**Acción:** usar claves canónicas compatibles con la igualdad numérica definida
en H01 y aplicarlas también a las particiones de joins que derraman a disco.

**Aceptación:** mismo multiconjunto de resultados para hash, index nested-loop y
spill; cubrir INNER/LEFT/RIGHT, duplicados, NULL y números no representables
exactamente como float. No resolverlo convirtiendo todos los enteros a `f64`.

### H03 · P0 · Node pierde enteros de 64 bits en las respuestas — reproducido

`jsonio.rs:43` serializa `Int64` como número JSON. El cliente usa `JSON.parse`
en `bindings/node/elitesql.js:130` y no recupera la precisión perdida.
La entrada `encodeParam(9007199254740993n)` es exacta gracias a la etiqueta
`int64`, pero el número de respuesta se convierte en `9007199254740992`.

**Acción:** transportar enteros fuera del intervalo seguro como valores
etiquetados con texto decimal; decodificarlos a `bigint` y actualizar tipos
TypeScript. Revisar también identidades devueltas, snapshots y búsquedas.
Definir cómo negociar/documentar el cambio de protocolo y la compatibilidad de
Python y FFI; los números dentro de JSON de usuario requieren una decisión
separada y explícita.

**Aceptación:** ida y vuelta real Node↔sidecar para los extremos de `i64`, tanto
en consultas normales como en cursores, parámetros e identidades.

### H04 · P0 · Backup no conserva la secuencia de identidad — reproducido

`backup.rs:47` copia esquemas y registros visibles, pero no copia explícitamente
el máximo histórico de las identidades. Se reconstruye a partir de las filas
que todavía existen.

Caso: tabla con `id int AUTO_INCREMENT PRIMARY KEY`, insertar `id=100`, borrarlo
y ejecutar backup. La siguiente inserción genera `101` en el origen y `1` en
la copia. El backup pasa la verificación actual. Restaurarlo puede reutilizar
identificadores que sistemas externos ya conocen.

**Acción:** capturar y persistir las secuencias junto con el estado consistente
del backup. Definir el tratamiento de identidades reservadas por transacciones
concurrentes y conservar el máximo seguro sin depender de filas supervivientes.

**Aceptación:** backup/restore con tabla vacía, máximo borrado, valores explícitos,
rollback y escritores concurrentes; siguiente identidad conforme al contrato
del origen. Ampliar los casos existentes de backup y relaciones (incluido el
ciclo de dos nodos en `sql_foreign_keys.rs`) con dependencias que crucen lotes
y secuencias cuyo máximo ya no esté presente.

### H05 · P1 · El sidecar omite los índices en SELECT simples — confirmado en código

`serve.rs:415` intenta primero `open_query_cursor()`. `QueryCursor::next()` en
`sql/exec.rs:107` siempre llama a `scan_batch_at_unbudgeted()`: no utiliza el
driver por id o índice que sí usa `Db::query()`.

Así, incluso `WHERE id = ?` y una igualdad sobre índice único pueden recorrer
toda la tabla desde Python/Node sidecar. Los benchmarks SQL del core no
representan ese costo de producción. H01 además demuestra una diferencia de
resultados entre ambas rutas.

**Acción:** compartir la selección del acceso con el cursor y proporcionar
probes compatibles con su snapshot. No reutilizar sin adaptación un lookup que
lea el último estado cuando el cursor promete una vista estable.

**Aceptación:** contador de filas visitadas que pruebe ausencia de full scan en
probes selectivos; pruebas de cambios concurrentes entre batches; benchmark
core/FFI/Unix/TCP sobre 10K, 100K y 1M filas. Separar latencia del motor y del
transporte y verificar el plan realmente ejecutado.

### H06 · P1 · El orden de AND determina si se usa un índice — reproducido

`table_driver_at()` en `sql/exec.rs:2037` devuelve la primera igualdad
convertible, aunque su columna no tenga índice. Con un índice sobre `email`:

```sql
EXPLAIN SELECT email FROM items WHERE category = 'x' AND email = 'a';
-- SCAN items (equality category = 'x', no index)

EXPLAIN SELECT email FROM items WHERE email = 'a' AND category = 'x';
-- INDEX LOOKUP items.email = 'a'
```

**Acción:** evaluar todos los candidatos antes de elegir: condición imposible,
id físico, índice único, índice secundario y escaneo. Después, incorporar
estimaciones de selectividad si los datos justifican la complejidad. La elección
de nested-loop para todo join indexado también merece un benchmark de cruce
con hash join, especialmente cuando la mayoría de las filas coincide.

**Aceptación:** permutar conjunciones no debe perder un acceso selectivo
disponible; planes y resultados verificables. Índices de rango/compuestos y
ordenación por índice quedan como evolución posterior, según cargas reales.

### H07 · P1 · Cursores retienen toda la admisión sin vencimiento — reproducido

Cada `QueryCursor` conserva un `MemoryPermit` de 16 MiB durante su vida. El
pool por defecto es 64 MiB (`db.rs:161`); `MemoryGovernor::acquire()` espera sin
deadline. `serve.rs:316` configura timeout de lectura sólo para transacciones,
no para cursores; `serve.rs:344` desactiva los timeouts antes de escribir una
respuesta autenticada.

Abrí cuatro cursores sin consumirlos. Una quinta consulta sobre una tabla de
dos filas seguía bloqueada a los 800 ms y respondió al cerrar el primer cursor.
La observación prueba retención de capacidad; la ausencia de vencimiento en el
código permite que la espera continúe mientras esos cursores sigan abiertos.

Además, salir con `break` de `for await` en Node no cierra el cursor:
`SidecarQueryCursor[Symbol.asyncIterator]()` carece de `finally`. En una prueba
con transporte simulado hubo cero `query_close` y `_cursorActive` siguió en true.

**Acción:** deadline/cancelación en admisión, vencimiento de cursores inactivos,
timeout de escritura y liberación al finalizar/cancelar. Agregar `try/finally`
al iterador Node y límites a su cola de solicitudes; manejar `socket.write()`
con backpressure y rechazar operaciones sobre clientes cerrados.

**Aceptación:** saturación devuelve un error acotado o espera dentro del plazo
configurado; desconexión, excepción y `break` liberan permisos/snapshots; un
cliente que deja de leer no retiene indefinidamente recursos del servidor.

### H08 · P1 · Spill abre todos los runs y los compara por fila — reproducido

`SpillSorter::for_each_sorted()` en `sql/exec.rs:1672` abre todos los archivos
temporales simultáneamente y selecciona el siguiente con `min_by` sobre todas
las cabeceras. Con N filas y R runs, la selección cuesta O(N·R); descriptores,
buffers de lectura y cabeceras crecen con R.

Con 2.000 filas, presupuesto de consulta de 512 bytes y límite de 128
descriptores en el proceso de prueba, `ORDER BY n DESC LIMIT 10` falló con
`Too many open files`. La limpieza posterior sí eliminó los temporales.

**Acción:** merge por heap y fan-in limitado, con pasadas intermedias y reservas
para buffers/cabeceras. Para `ORDER BY ... LIMIT k`, evaluar un heap top-k cuando
`offset + k` quepa; hoy se ordenan buffers completos antes de truncarlos.

**Aceptación:** resultado exacto con miles de runs bajo un límite de 128
descriptores; memoria de merge contabilizada; errores de disco sin archivos
huérfanos. Medir CPU, bytes temporales y pasadas, no sólo tiempo total.

### H09 · P1 · El presupuesto no cubre uniformemente la memoria — revisión estática

Hay tres huecos concretos:

- `memory.rs:119` hace `bytes.min(capacity)` en `acquire_unbounded()`: un heap
  congelado mayor que el pool se registra por debajo de su tamaño solicitado,
  pese al comentario que promete contabilizarlo completo.
- `restore()` en `backup.rs:186` materializa `db.scan()` para contar registros;
  `export` en `main.rs:240` materializa toda la tabla antes de escribirla.
- Los límites de lotes son principalmente por filas. Registros anchos y las
  representaciones simultáneas de registros, JSON y bytes pueden superar
  ampliamente la reserva estimada por consulta.

Esto no demuestra que el proceso exceda siempre 384 MiB. Ese presupuesto tampoco
es un límite estricto de RSS: resultados del llamador, mmap, caché del sistema,
pilas y buffers de transporte deben distinguirse de la memoria contabilizada.

**Acción:** contabilizar transferencias de ownership sin truncar tamaños,
reservar antes de asignar donde sea posible, introducir lotes por bytes y
recorrer restore/export por batches. Medir memoria total y la del gobernador
por separado, incluyendo solapamiento de mantenimiento.

**Aceptación:** contadores sin subregistro deliberado; restore/export con memoria
de trabajo proporcional al lote; cargas de registros anchos y consultas
concurrentes bajo límites de memoria explícitos.

### H10 · P1 · Una respuesta grande consume el cursor sin entregar sus filas — reproducido

`serve.rs:438` reúne filas por cantidad; `query_next` avanza o elimina el cursor
antes de aplicar `bounded_response_bytes()` en `serve.rs:348`.

Inserté tres textos de 3 MiB mediante solicitudes separadas. `query_next` con
`max_rows=3` devolvió el error de 8 MiB. Al reintentar con una fila respondió
`no active streaming cursor`: había llegado al final sin entregar el batch.
El error recomienda usar el mismo protocolo de streaming que ya se está usando.

**Acción:** construir batches con presupuesto de bytes serializados, conservar
la fila pendiente que no cabe y definir un error preciso para una sola fila
demasiado grande. Evitar duplicar un batch completo sólo para descubrir el
exceso al serializarlo.

**Aceptación:** variar el ancho y `max_rows` no pierde ni duplica filas; límites
de byte comprobados antes de consumir irrevocablemente el batch; comportamiento
documentado para una fila que excede el máximo de respuesta.

### H11 · P2 · Costo de commits y mutaciones con índices — oportunidad por medir

El reporte versionado `benchmark-results/current-acceptance-2026-09-05.md`
señala:

- Carga sostenida de 1M filas en transacciones de 1.000: 0,855 s EliteSQL frente
  a 0,745 s SQLite en tiempo total; throughput 12,9% menor.
- Con 16 lectores y cuatro escritores, perfil insert: 113.863 escrituras/s;
  perfil con índices derivados: 46.518. Son perfiles diferentes y su diferencia
  no aísla por sí sola el costo de cada índice.
- El rendimiento y agrupamiento Safe con dos escritores varían entre corridas.
  La preasignación WAL de 64 MiB no mostró mejora medible en ese reporte.

`prepare_commit()` (`db.rs:8339`) todavía crea un `Arc` de payload por registro.
La ruta coordinada excluye índices, identidades y claves foráneas; las
identidades además usan la codificación WAL bajo el mutex global
(`db.rs:8864`). Son candidatos a profiling, no causas cuantificadas aquí.

**Acción:** medir asignaciones por fila, preparación fuera/dentro del lock,
espera, validación, append, sync, apply y deuda de mantenimiento. Ensayar un
arena por commit, reutilización de buffers y reducción del trabajo dentro del
lock sólo después de identificar el costo dominante.

**Aceptación propuesta:** mejora reproducible de al menos 10% en el objetivo
seleccionado y sin regresión p99 mayor al 5% fuera de la variabilidad de la
línea base. Son metas de decisión, no mejoras prometidas. Mantener pruebas de
crash, atomicidad, conflictos y durabilidad equivalente.

### H12 · P1/P2 · Falta una puerta reproducible de calidad y rendimiento

No hay workflows de CI versionados en `.github/workflows` en esta revisión,
aunque `plan.md` los contempla. `cargo fmt --all -- --check` detecta diferencias
en `db.rs`, `distance.rs` y `vector.rs`; Clippy con advertencias como errores pasa.

El informe de aceptación del 5 de septiembre declara que mide un árbol sucio
sobre `849190f`, y que ese commit por sí solo no reproduce los resultados.
Además, sus pruebas de caché «cold» en macOS no expulsan la caché del sistema.
No deben usarse como prueba de I/O verdaderamente frío ni atribuirse sin más al
HEAD auditado.

**Acción:** versionar un comando de aceptación y automatizar fmt, Clippy, Rust,
build FFI, Python y Node. Incluir roundtrips reales de clientes, no sólo
encoding. Registrar SHA, diff si existe, opciones, hardware, energía, semillas,
perfil y repeticiones de cada benchmark. Ejecutar rendimiento secuencialmente
en un entorno estable; pruebas funcionales pueden ejecutarse con concurrencia.

**Aceptación:** una revisión limpia se reproduce con los comandos documentados;
ninguna métrica mezcla durabilidad o perfiles de carga. Añadir corpus de formato
y recuperación entre versiones si se ofrece compatibilidad persistente.

## Plan de ejecución

Las estimaciones son días de ingeniería de una persona familiarizada con Rust;
se revisan al terminar cada etapa. Cada entrega debe tener un alcance pequeño
y verificable. No es necesario reescribir el motor ni cambiar todo el formato.

| Etapa | Alcance y entregables | Dependencia | Esfuerzo orientativo |
|---|---|---|---|
| 0 | Fijar baseline, guardar reproducciones como regresiones, comando de aceptación y formato/CI inicial (H12) | Ninguna | 1–2 días |
| 1 | Contrato numérico único, joins coherentes y transporte int64 exacto (H01–H03); backup de secuencias (H04) | Etapa 0 | 4–7 días |
| 2 | Liberación/cancelación de cursores, admisión con deadline y batches por bytes (H07, H10) | Baseline; coordinar protocolo con etapa 1 | 3–5 días |
| 3 | Driver compartido y compatible con snapshots; selección de índice independiente del orden (H05–H06) | Contrato de igualdad de etapa 1 | 3–5 días |
| 4 | Merge con fan-in limitado, contabilidad completa y restore/export por lotes (H08–H09) | Métricas y regresiones de etapa 0 | 4–7 días |
| 5 | Profiling y optimizaciones de commits; evaluación de top-k y estrategias de join (H11) | Etapas anteriores estabilizadas | 3–5 días |
| 6 | Modularizar por responsabilidades y consolidar documentación/benchmarks | Después de estabilizar comportamiento | 2–4 días |

Total orientativo: **20–35 días de ingeniería**, con entregas útiles desde la
primera etapa. No comprometer las optimizaciones experimentales de la etapa 5
si el profiling no demuestra beneficio.

En la etapa 6 conviene separar `db.rs` (14.585 líneas) en coordinación de commits,
lectura/MVCC y mantenimiento; `sql/exec.rs` (4.988 líneas) en comparación/typing,
planificación y operadores. Hacerlo sin alterar simultáneamente semántica y
layout persistente facilita revisar invariantes de locks y recuperación.

## Matriz de validación

| Área | Casos mínimos | Evidencia de salida |
|---|---|---|
| Exactitud SQL | Límites numéricos; índices presentes/ausentes; distinto orden AND; joins hash/index/spill | Resultados equivalentes para planes equivalentes |
| Clientes | Core/FFI/Unix/TCP; Node bigint; Python int; cierre y excepción | Roundtrips exactos y recursos liberados |
| Backup | Máximo borrado, tabla vacía, identidades concurrentes, FK y blobs | Datos y próxima identidad correctos tras restore |
| Recursos | 1/4/8/128 conexiones; lectores lentos; cursores abandonados; filas de MiB | Espera acotada, memoria observada y sin pérdida de filas |
| Spill | Muchos runs, 128 descriptores, disco lleno, cancelación, top-k y offset | Resultado exacto, fan-in acotado y limpieza |
| Storage | Safe/Balanced/Fast, snapshots retenidos, kill -9 y reapertura | Sin pérdida de commits confirmados bajo la garantía de cada modo |
| Rendimiento | 10K/100K/1M, cardinalidad/selectividad/ancho variables, índices derivados | p50/p95/p99, throughput, filas visitadas, memoria, bytes de spill y tiempos de locks |

Mantener recall y latencia ANN como control de regresión. Las optimizaciones
HNSW recientes ya incluyen persistencia de runs y prefetch; no priorizar otra
ronda de microoptimización vectorial antes de resolver los problemas anteriores.

## Verificaciones de esta auditoría

- `cargo test --workspace --locked`: 376 pruebas pasan, cero fallos y cero
  ignoradas, incluyendo doc-tests; proceso finalizado con código 0.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: pasa.
- `cargo fmt --all -- --check`: falla por diferencias de formato en tres archivos.
- `cargo build --locked -p elitesql-ffi`: pasa.
- `python3 -m unittest discover -s bindings/python/tests -v`: siete pruebas pasan,
  repetidas después de construir la librería FFI del código actual.
- `node bindings/node/test.js`: pasa.
- Reproducciones adicionales: precisión SQL, divergencia de joins, secuencia de
  backup, plan según orden AND, agotamiento de admisión, límite de descriptores
  de spill, respuesta de cursor demasiado grande y comportamiento Node.

Las pruebas existentes pasando no contradicen estos hallazgos: las reproducciones
ejercitan combinaciones y límites que no están cubiertos por sus aserciones.
