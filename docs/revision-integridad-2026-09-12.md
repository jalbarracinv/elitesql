# Revisión de integridad de datos de EliteSQL (tercera pasada)

Revisión de código: `3f7267b1e50a0a0b62e0ae185e80a5fa14b449ce` (árbol limpio).
Fecha: 2026-09-12, zona America/Lima.

> **Estado (2026-09-12, misma fecha):** los 19 hallazgos están corregidos en el
> árbol de trabajo; el detalle de cada corrección, sus pruebas y la medición
> de rendimiento están en [implementacion-revision-2026-09-12.md](implementacion-revision-2026-09-12.md).

**Conclusión: las correcciones I01–I08 y H01–H04 de las revisiones anteriores
están implementadas y se verificaron en código. El núcleo canónico (WAL,
manifest, segmentos, checkpoint, compactación) es sólido. Quedan un fallo P0
reproducido en la recuperación de DDL que desactiva UNIQUE en todo el proceso,
varios P1 en recuperación ante pérdida de energía, índices vectoriales,
clientes y herramientas, y un conjunto de límites del contrato de durabilidad
que la documentación describe con más optimismo del que el código garantiza.**

Alcance: `elitesql-core` completo (db.rs, db/commit.rs, db/maintenance.rs,
wal.rs, manifest.rs, run_manifest.rs, segment.rs, paged.rs, vector.rs,
text.rs, check.rs, repair.rs, backup.rs, ddl.rs, schema.rs, sql/*), FFI,
sidecar, CLI, bindings Python y Node, suite de pruebas y el procedimiento de
despliegue `ActualizarNAS.md`. Se usaron bases temporales para las tres
reproducciones marcadas como tales. No se modificó código del motor.

Prioridades: **P0** pérdida, duplicado o sobrescritura silenciosa de datos
confirmados; **P1** pérdida o indisponibilidad visible con error, o error que
induce al cliente a producir datos erróneos; **P2** endurecimiento.

## Resumen

| ID | Prioridad | Área | Situación | Estado |
|---|---|---|---|---|
| R01 | P0 | Recuperación DDL | Reapertura con `ADD COLUMN` pendiente sirve sin ningún índice derivado: UNIQUE no se aplica | Reproducido |
| R02 | P1 | WAL / pérdida de energía | Una cola rellena de ceros o basura (no un prefijo exacto) se clasifica como corrupción y la apertura se rechaza | Confirmado en código |
| R03 | P1 | WAL / fsync | Un `fsync` fallido devuelve `CommitUnknown` pero no valla ni rota el escritor; el siguiente commit se confirma como durable encima de un registro posiblemente perdido | Confirmado en código |
| R04 | P1 | Índice vectorial | Tras una fusión total de runs, el catch-up al abrir omite los commits entre la generación del manifest y el sello del run: vectores ausentes en ANN | Confirmado en código |
| R05 | P1 | Cliente Python sidecar | Con `timeout` configurado, un `socket.timeout` desincroniza petición/respuesta: cada llamada posterior recibe la respuesta de la anterior | Confirmado en código |
| R06 | P1 | Contrato de errores | `CommitUnknown` (17) y `Io` (1) por desconexión describen commits que sí pueden estar publicados; un reintento de la aplicación duplica filas. Sin documentación en el README de Python | Confirmado |
| R07 | P1 | SQL | `UPDATE` dentro de una transacción explícita falla con `Corrupt("record has non-text id")` en tablas con `id int AUTO_INCREMENT PRIMARY KEY` | Reproducido |
| R08 | P1 | API Rust/FFI | Crear una tabla llamada `"\0elitesql_identity"` deja la base inabrible | Reproducido |
| R09 | P1 | DDL concurrente | `ADD COLUMN ... NOT NULL DEFAULT v` puede publicar NOT NULL sobre una fila con NULL insertada entre el backfill y la publicación final | Sospechado (lectura) |
| R10 | P1 | CLI | `import` confirma por lotes de 1000 y no es idempotente; `repair` con `skipped > 0`, `restore` y `check` con advertencias salen con código 0 | Confirmado |
| R11 | P1 | Node | `±inf` se decodifica como `NaN` y se reescribe; timestamps truncados a milisegundos; `close()` rechaza peticiones ya enviadas que el servidor sí ejecuta | Confirmado |
| R12 | P1 | Despliegue NAS | Credenciales en texto plano en la guía local (ignorada por git, copiada al NAS por el `tar`); sin `backup`/`check` previo a la actualización; rollback del `.so` sin considerar `format_version`; bucle de parada por PID frágil | Confirmado |
| R13 | P2 | Durabilidad | `Balanced` no tiene temporizador: sin nuevos commits nada se sincroniza; el cierre limpio no sincroniza el WAL en `Fast`/`Balanced`; sin `F_FULLFSYNC` en macOS | Confirmado en código |
| R14 | P2 | WAL | Tras recuperar con sucesores ya creados, el escritor reanuda en el último WAL sin republicar `required_wal_id` | Confirmado en código |
| R15 | P2 | Claves | `-0.0`/`0.0` y NaN: claves de índice distintas para valores que `=` considera iguales (o viceversa) | Confirmado |
| R16 | P2 | DDL | `DROP COLUMN` acepta la columna identidad/PK; `DROP TABLE` deja la secuencia en el manifest; parámetro texto en columna `json` difiere del literal | Confirmado |
| R17 | P2 | Índices derivados | Un error transitorio al leer un run manifest borra todos los runs vivos de ese índice; `publish_vector_merge` puede desenlazar runs sin republicar | Confirmado / sospechado |
| R18 | P2 | Lectura | La apertura `--read-only` convierte un WAL ilegible o un segmento ausente en estado vacío sin señalarlo al llamador | Confirmado |
| R19 | P2 | Pruebas | Sin simulación de pérdida de energía, sin `kill -9` en `Fast`/`Balanced`, sin ENOSPC, sin write skew, sin fsync fallido de extremo a extremo | Confirmado |

Rutas relativas a `crates/elitesql-core/src/` salvo indicación.

## R01 · Recuperación de `ADD COLUMN` sin índices derivados (P0)

Referencias: `db.rs:4297-4300`, `db.rs:5516-5545` (`apply_ddl`),
`db.rs:5583-5620` (`apply_add_column`), `db.rs:5710-5724`
(`finish_ddl_recovery`), `db/commit.rs:1283` (`validate_unique`),
`db.rs:8336-8344` (`insert_if_unique`).

Cuando `ddl.json` existe al abrir, `open_with` inicializa `secondary`,
`vector` y `text` como mapas vacíos con el comentario "rebuilt by the DDL
replay". Eso es cierto para `RENAME`/`DROP COLUMN`, que pasan por
`rewrite_segments` y `rebuild_derived_indexes_after_rewrite`. Para `ADD COLUMN`
la ruta es `apply_add_column` (publica catálogo y hace backfill con commits
ordinarios) seguida de `finish_ddl_recovery`, cuyo comentario promete
"rebuild every derived index" pero cuyo cuerpo sólo hace checkpoint y limpia
huérfanos. El proceso queda con **cero índices derivados en todas las tablas**
hasta la siguiente reapertura.

`validate_unique` e `insert_if_unique` hacen `if let Some(index) =
st.secondary.get(..)`: índice ausente significa comprobación omitida, no error.
`search_text` y la búsqueda vectorial devuelven "no index" o resultados vacíos.

Reproducción (base temporal): tabla con índice único en `email`, fila
`a@x`; se escribe un `ddl.json` de `AddColumn` y se reabre. El duplicado
`a@x` es **aceptado**; tras checkpoint y segunda reapertura el índice
reconstruido contiene ambas filas. El daño es permanente y `check` sólo lo
detecta con la verificación lógica profunda.

**Corrección:** en `finish_ddl_recovery` cargar/reconstruir los tres tipos de
índice con la misma ruta que la apertura normal, y hacer que
`validate_unique`, `insert_if_unique` y `final_ids_matching` devuelvan
`Corrupt` cuando el catálogo declara un índice que el estado no tiene, en lugar
de omitir la comprobación. Regresión: `kill -9` durante cada variante de DDL
seguido de inserción duplicada, búsqueda de texto y ANN.

## R02 · Cola del WAL con ceros se trata como corrupción (P1)

Referencias: `wal.rs:197-260` (`scan_wal`), `wal.rs:265-345`
(`parse_record`), `wal.rs:36-97` (`validate_wal_chain`), `db.rs:4226-4244`.

La corrección de I06 distingue "registro incompleto" (prefijo estricto de un
registro) de "corrupción" (CRC inválido, kind desconocido, secuencia
inválida). Un `kill -9` siempre deja un prefijo exacto, y todas las pruebas de
crash usan `kill -9`. Una pérdida de energía real no: según el sistema de
archivos, la última extensión del archivo puede quedar rellena de ceros o con
un bloque parcialmente escrito. Con ceros, `parse_record` lee `version=0`,
`count=0` y compara el CRC almacenado `0` con el CRC de 12 bytes cero: **"wal:
record crc mismatch" → `Corrupt`**. Con un byte `kind = 0`: "unknown change
kind 0". En ambos casos `validate_wal_chain` rechaza la apertura normal y
exige `repair` hacia un destino nuevo, aunque el estado sea exactamente el que
la semántica de `Balanced`/`Fast` (y de `Safe` para el último commit no
reconocido) considera legítimo.

Esto no pierde datos, pero convierte un escenario contemplado por el contrato
en una indisponibilidad que requiere intervención manual, y las pruebas no
pueden observarlo porque no simulan pérdida de energía (ver R19).

**Corrección:** tratar como cola incompleta (truncable) una región final cuyos
bytes restantes son todos cero, y considerar torn un último registro con CRC
inválido cuando no existe ningún registro válido posterior ni sucesor WAL con
contenido (la heurística actual ya hace lo contrario: busca un sucesor válido
para *probar* corrupción). Documentar la política por modo de durabilidad.
Añadir pruebas que rellenen la cola con ceros y con basura.

## R03 · `fsync` fallido no valla el escritor (P1)

Referencias: `wal.rs:503-525` (`sync_data`), `db/commit.rs:53`, `:720`,
`:1170-1175`; contraste con `db/maintenance.rs:111-122`
(`publication_sync_error` sí valla).

Cuando `sync_data` falla, el commit devuelve `CommitUnknown` (correcto), pero
el `WalWriter` sigue utilizable, no se rota el WAL y no se llama a
`fence_writes`. En Linux, tras un `EIO` en `fsync`, las páginas sucias se
descartan y se marcan limpias; el siguiente commit se anexa detrás del
registro posiblemente perdido y su `fsync` tiene éxito, por lo que **se
reconoce como durable un commit que depende de otro no durable**. Tras un
crash, `validate_wal_chain` detecta el hueco (CRC o versión) y rechaza abrir:
no hay pérdida silenciosa, pero sí un commit reconocido perdido y una base que
requiere `repair`.

**Corrección:** tras `SyncFailed`, vallar escrituras hasta reabrir (como ya
hace la publicación de manifests) o, como mínimo, forzar rotación de WAL y
re-sincronización antes de aceptar más commits. Prueba de extremo a extremo
con inyección de fallo de sync a través de `Txn::commit` y reapertura.

## R04 · Catch-up vectorial omite commits tras una fusión total (P1)

Referencias: `db.rs:9726` (sello del run fusionado), `db.rs:9765-9776`
(republicación del manifest a la generación anterior), `db.rs:9849-9887`
(`load_vector_run_set` devuelve `floor = manifest.runs.first().generation`),
`db.rs:10064` (`catch_up_vector_index` retorna si `last.version <= floor`).

Un merge puede seleccionar **todos** los runs de un índice y sellar el
resultado con `snapshot_version = committed_version` (S), republicando el
manifest a la generación previa G < S. Al abrir, `floor` pasa a ser S mientras
la cobertura real del manifest es G. Los commits en (G, S] cuyos vectores sólo
vivían en el overlay mutable no se reexaminan: filas nuevas quedan fuera del
ANN y una fila actualizada (cuyo id fue retirado de los runs mapeados por
`VecIdx::insert`) desaparece del índice. Los datos canónicos están intactos;
el índice queda incompleto hasta una reconstrucción.

**Corrección:** `floor = min(first.generation, manifest.generation)`.
Regresión: cuatro runs similares, commits posteriores, merge, crash antes del
flush, reapertura y comparación de recall con fuerza bruta (hoy
`vector_crash.rs` sólo comprueba `!hits.is_empty()`).

## R05 · El cliente Python del sidecar se desincroniza tras un timeout (P1)

Referencias: `bindings/python/elitesql.py:756-778`, `:805-823` (`_call`);
`crates/elitesql-cli/src/serve.rs:547`, `:813`.

`_call` escribe una línea y lee una línea. El servidor devuelve un `id` de
petición pero el cliente no lo envía ni lo comprueba. Si el usuario configuró
`timeout`, `readline()` lanza `socket.timeout`, nada marca la conexión como
rota y la respuesta tardía queda en el socket. La siguiente llamada lee la
respuesta anterior: un `SELECT` recibe `{"inserted": [...]}`, y dentro de
`SidecarTransaction` un `rollback()` "tiene éxito" con una respuesta ajena
mientras el servidor aún no ha revertido. A partir de ahí toda la sesión
devuelve resultados desplazados. Node no está afectado (sin timeout por
petición; `_failAll` cierra la conexión).

**Corrección:** enviar `id` y verificarlo en la respuesta; ante timeout cerrar
la conexión y marcar el cliente como inutilizable. Prueba con transporte
simulado que retrase una respuesta.

## R06 · Semántica de `CommitUnknown` e `Io` frente a reintentos (P1)

Referencias: `db/commit.rs:1170-1174`, `:727`; `crates/elitesql-ffi/src/lib.rs:48-51`;
`serve.rs:811`; `elitesql.py:531`, `:868`; `bindings/python/README.md`.

El código 17 significa "la versión **ya está publicada**, su durabilidad es
incierta". El código 1 (`Io`) se devuelve también por desconexión del sidecar
tras ejecutar el commit y antes de leer la respuesta (reinicio del servidor,
timeout de escritura de 30 s en `serve.rs:344-353`). Un patrón habitual en la
aplicación (`except EliteSQLError: retry`) duplica filas en ambos casos. Los
bindings sólo nombran los códigos 9, 17 y 18; el README de Python, que es el
cliente de producción, no menciona códigos de error ni política de reintento.
Además el deadline **absoluto** de 30 s por transacción del sidecar
(`serve.rs:40`, `:606-621`) revierte y devuelve `InvalidArgument` (8), no
distinguible de un argumento malformado.

**Corrección:** documentar en los bindings qué códigos admiten reintento (sólo
9) y cuáles exigen verificación posterior (17, 1 por desconexión); añadir
constantes para 2, 10 y 13; código específico para el vencimiento de
transacción; considerar idempotencia opcional (clave de petición) en el sidecar.

## R07 · `UPDATE` transaccional falla con PK entera (P1)

Referencia: `sql/exec.rs:3994-3997` toma la clave física de `record["id"]`,
pero `Txn::scan` (`db.rs:8130-8150`) sólo inyecta el ULID físico cuando
`schema.has_implicit_id()`.

Con `CREATE TABLE users (id int AUTO_INCREMENT PRIMARY KEY, n int)`, todo
`tx.query("UPDATE users SET n = n + 1")` dentro de una transacción explícita
devuelve `Corrupt("record has non-text id")`. En autocommit funciona;
`exec_delete_txn` usa correctamente la tupla `(id, record)`. El savepoint
revierte, así que no hay datos parciales, pero la ortografía de PK documentada
en el manual no puede actualizarse en transacciones y el error se reporta
como corrupción. `tests/txn.rs` usa `user_id`, por eso no se detectó.

**Corrección:** usar la clave de la tupla devuelta por `scan`. Regresión con
identidad llamada `id` en INSERT/UPDATE/DELETE/RETURNING transaccionales.

## R08 · Nombre de tabla reservado accesible por API (P1)

Referencias: `schema.rs:369-375`, `db.rs:4556` (`create_table`),
`wal.rs:11`, `:296`.

La marca de agua de identidades se multiplexa en el WAL como puts sobre la
pseudo-tabla `"\0elitesql_identity"`. `TableSchema::validate` sólo comprueba
longitud y no vacío. Desde la API Rust/FFI (el lexer SQL sólo admite ASCII)
`create_table("\0elitesql_identity", ...)` + `insert` deja un WAL que la
siguiente apertura rechaza con `Corrupt("wal: invalid identity metadata")`.
Un payload de exactamente 8 bytes se leería en silencio como marca de agua.

**Corrección:** rechazar nombres con caracteres de control o el prefijo
reservado en `TableSchema::validate` y en `decode` de catálogo.

## R09 · `ADD COLUMN ... NOT NULL` con inserción concurrente (P1, sospechado)

Referencia: `db.rs:5583-5620`.

La columna se publica como nullable, `backfill_column` corre en commits
ordinarios sin el mutex de commit, y la publicación final pone
`nullable=false` sin re-escanear. Otra conexión puede insertar un NULL
explícito en la columna nueva (o una fila con id textual por debajo del cursor
del backfill) en esa ventana. Resultado: columna NOT NULL con un NULL
persistido; el siguiente `UPDATE` de esa fila falla en `check_value`.

**Corrección:** bajo el mutex final, verificar que no quedan filas con NULL
en la columna (tratando NULL explícito como pendiente) antes de publicar; si
las hay, repetir el backfill o abortar.

## R10 · Herramientas CLI: parcialidad y códigos de salida (P1)

Referencias: `crates/elitesql-cli/src/main.rs:560-607` (`import`),
`:222-236` (`repair`), `:210-221` (`restore`), `:170-185` (`check`).

- `import` confirma cada 1000 filas; una línea inválida deja
  `⌊(N−1)/1000⌋·1000` filas confirmadas, y reejecutar el archivo duplica todas
  las filas sin `id` explícito.
- `repair` sale con 0 aunque `skipped > 0`; `restore` y `check` salen con 0
  con advertencias, que van a stdout. Los scripts de operación no pueden
  detectar una recuperación parcial ni un índice derivado inconsistente
  (`check` reporta como advertencia que el índice primario discrepa de la fila
  canónica).

**Corrección:** `import` en una transacción o con modo explícito
`--batch` documentado e idempotente por `id`; códigos de salida distintos para
"ok", "ok con advertencias" y "pérdida parcial"; advertencias a stderr.

## R11 · Fidelidad de valores en Node (P1)

Referencias: `bindings/node/elitesql.js:24-39`, `:86-103`, `:151`, `:201-228`,
`:320-323`; `jsonio.rs:51-58`.

- `±inf` se serializa como `{"$t":"float64","repr":"inf"}`; `Number("inf")`
  es `NaN`. Un read-modify-write convierte infinito en NaN en disco.
- Timestamps se truncan a milisegundos en ambas direcciones; `time` se
  devuelve como número sin codificador de vuelta.
- Contenido de columnas `json` se pasa crudo: un entero > 2^53 dentro del JSON
  se redondea en `JSON.parse`.
- `close()` invoca `_failAll`: las peticiones ya enviadas se reportan como
  fallidas aunque el servidor las ejecute (un `INSERT` autocommit se
  confirma y el llamador ve error).
- El tipo de int64 depende de la magnitud (`Number` o `BigInt`), lo que hace
  que `===` y la reserialización se comporten distinto según el valor.

Python maneja correctamente los tres primeros casos.

## R12 · Procedimiento de despliegue en el NAS (P1 operativo)

Referencia: `ActualizarNAS.md`.

- Las líneas 8-9 contienen usuario y contraseña del NAS en texto plano, en
  contradicción con la propia guía (línea 18). El archivo está en `.gitignore`
  y no se ha confirmado nunca, así que no está en el historial de git; sí
  viajó al NAS con el `tar` del árbol de trabajo del paso 3 (directorios
  `.build-elitesql-*`). Eliminarla del archivo y revisar esas copias.
- No se toma `elitesql backup` ni se ejecuta `elitesql check` sobre la base de
  producción antes de sustituir la biblioteca. Un fallo en la nueva versión
  (p. ej. R01 tras un DDL) no tiene punto de retorno verificado.
- La sección de rollback restaura el `.so` anterior sin considerar que una
  versión nueva puede haber escrito un `format_version` o un formato de índice
  V3 que la anterior no lee (el motor rechaza la apertura, no corrompe, pero el
  rollback deja el servicio caído).
- El bucle de parada usa `pgrep -f ... | head -1` y `kill -TERM` sobre el
  primer PID; con varios workers puede matar a un worker, el master lo
  relanza, y el arranque posterior abre un segundo master. El `flock` del
  motor haría fallar al segundo con código 10, así que es visible, no
  corrupto, pero deja el servicio en estado indefinido.
- Se empaqueta el árbol de trabajo (`tar` del directorio), por lo que cambios
  no confirmados viajan bajo un `REVISION` aparentemente limpio.
- La guía de inicio ya advierte contra NFS/SMB; conviene añadir a este
  procedimiento una comprobación de que el directorio de la base está en disco
  local del NAS y no en un volumen compartido.

## R13 · Contrato de durabilidad frente a implementación (P2)

Referencias: `wal.rs:489-499` (`sync_due`), `db.rs:3284-3312` (`Drop for Db`),
`README.md:459-461`, `docs/getting-started.md:171-177`.

- `Balanced` sincroniza sólo cuando llega un commit **después** de 25 ms del
  último sync. No hay temporizador: si la aplicación hace un commit y queda
  inactiva, ese commit permanece sin sincronizar indefinidamente. "Pierde los
  últimos ~25 ms" debería leerse "pierde todo desde el último sync".
- `Drop for Db` (cierre limpio) drena los workers y publica índices derivados,
  pero **no sincroniza el WAL ni hace checkpoint**. En `Fast`/`Balanced`, un
  `close()` seguido de caída del sistema pierde commits reconocidos. Está
  dentro del contrato literal, pero contradice la expectativa habitual de que
  cerrar hace durable.
- No hay `F_FULLFSYNC` en macOS; `fsync` allí no vacía la caché del disco, así
  que `Safe` no sobrevive a un corte eléctrico en macOS. Producción es Linux,
  pero las pruebas de CI y los benchmarks corren también en macOS.

**Corrección:** temporizador de sync en `Balanced`; `sync_data` del WAL en el
cierre limpio para todos los modos; `F_FULLFSYNC` bajo `cfg(target_os =
"macos")`; ajustar la tabla de durabilidad del README.

## R14 · `required_wal_id` tras recuperación con sucesores (P2)

Referencias: `db.rs:4219-4272` (bucle de replay), `db.rs:4340`
(`WalWriter::open(&dir, active_wal_id)`), `db/maintenance.rs` (checkpoint en
segundo plano crea puente y activo antes de publicar la reserva).

Si el proceso muere después de crear los WAL `N+1` (puente) y `N+2` (activo)
pero antes de publicar el manifest con `required_wal_id = N+2`, la reapertura
reproduce `N`, `N+1`, `N+2` y reanuda las escrituras en `N+2` mientras el
manifest sigue diciendo `required_wal_id = N`. La ventana de "sucesor final
ausente no demostrable" que I06 cerró para rotaciones nuevas se reabre para
esa incarnación hasta el siguiente checkpoint. Requiere daño de archivos para
materializarse; se clasifica P2.

**Corrección:** si `active_wal_id > manifest.required_wal_id` tras el replay,
republicar el manifest con la extensión real antes de aceptar commits.

## R15 · Claves de coma flotante (P2)

Referencias: `value.rs:326-341` (`encode_value` usa bits LE), `db.rs:10720`
(`index_key`), `sql/values.rs:185` (`total_cmp`).

`0.0` y `-0.0` generan claves de índice distintas; en el ejecutor SQL
`total_cmp` también los distingue, así que índice y scan coinciden entre sí
pero no con IEEE `=`. `Value` deriva `PartialEq` con `==` de f64, usado en
`final_ids_matching` para FK: un padre con `-0.0` no se encuentra para un hijo
con `0.0` (falso error de FK) y una columna Float64 UNIQUE acepta ambos. NaN no
se rechaza en Float64 (`check_value` sólo lo hace en vectores): dos NaN
colisionan en un índice único aunque `=` diga que difieren.

**Corrección:** canonicalizar `-0.0` a `0.0` y rechazar o normalizar NaN al
insertar y en `index_key`; unificar la igualdad usada por índices, FK y SQL.

## R16 · Otros límites de DDL y tipos (P2)

- `drop_column` (`db.rs:5364-5410`) no protege `column.identity`: `ALTER TABLE
  users DROP COLUMN id` tiene éxito y `SELECT *` pasa a exponer el ULID físico
  como `id`. I02 protegió el índice, no la columna.
- `drop_table` (`db.rs:5139-5141`): se revisó en detalle durante la
  corrección; `publish_catalog_generation_locked` ya poda del manifest las
  identidades de tablas que desaparecen del catálogo, así que el valor antiguo
  no vuelve tras reabrir. Sin cambio necesario.
- Un parámetro texto ligado a una columna `json` se guarda como cadena JSON
  (`sql/values.rs:94-96`), mientras el literal equivalente se parsea como
  objeto (`:19-23`). Misma sentencia, distinto valor almacenado, en contra del
  manual.
- `Int → Float64` redondea en silencio por encima de 2^53; `Int → Date`
  acepta cualquier i32 de días sin la validación de calendario que sí aplica a
  cadenas (`sql/values.rs:15`, `:43-48`).
- Las cascadas se expanden en commit (`db.rs:8472`): dentro de la transacción,
  tras `DELETE` del padre, los hijos siguen visibles y un `INSERT` de un hijo
  para la clave borrada sólo se rechaza al `COMMIT`.

## R17 · Limpieza de runs derivados (P2)

- `DerivedRunManifest::referenced_files` (`run_manifest.rs:325`) usa
  `fs::read(path).unwrap_or_default()`; un error transitorio (EMFILE, EIO)
  sobre un manifest válido hace que `cleanup_orphan_sidx/tidx` y
  `cleanup_primary_run_orphans` borren todos los runs vivos del índice. Seguro
  por "los índices son desechables", pero convierte un problema de E/S en una
  reconstrucción completa silenciosa.
- `publish_vector_merge` (`db.rs:9765-9776`): si `durable_generation` pasó a
  `None` entre la planificación y la publicación, no se escribe manifest pero
  los runs antiguos sí se desenlazan. La siguiente apertura reconstruye; se
  pierde la garantía de evitar la reconstrucción.

## R18 · Apertura de sólo lectura sin señal de parcialidad (P2)

Referencias: `db.rs:4041-4046`, `db.rs:4232-4236`.

En `--read-only`, un segmento ausente se omite y un WAL ilegible se trata
como vacío. Es la semántica deseada para inspección, pero el handle no expone
ningún indicador de que el estado servido es parcial; un `export --read-only`
usado como respaldo puede quedar incompleto sin aviso.

## R19 · Cobertura de pruebas (P2, pero condiciona a R02, R03 y R13)

Matriz resumida de lo que sí y no está cubierto (archivos en
`crates/elitesql-core/tests/`):

| Escenario | Cobertura |
|---|---|
| `kill -9` en commit `Safe`, DDL, publicación vectorial | Sí (`crash_kill.rs`, `ddl_crash.rs`, `vector_crash.rs`) |
| `kill -9` en `Fast`/`Balanced` | No |
| Pérdida de energía (descartar escrituras no sincronizadas, colas con ceros, reordenación) | No, en ningún modo |
| Cola WAL torn, corrupción interior, sucesor ausente, manifest.prev | Sí |
| Alteraciones de bits en índices | V3 exhaustivo en metadatos; sin payload/clave, sin V1, sin sidx/tidx/vidx |
| ENOSPC, fsync fallido de extremo a extremo | No (sólo unitario en `wal.rs`) |
| Conflicto escritor-escritor, UNIQUE/FK/identidad concurrentes | Sí |
| Write skew | No |
| `kill -9` durante backup/restore/repair; matar al proceso que recupera | No |
| Dos procesos vivos compitiendo por el LOCK; NFS/SMB | No |
| `format_version` distinto | No |

Oráculos débiles: `vector_crash.rs` sólo exige `!hits.is_empty()`;
`backup_is_snapshot_consistent_under_concurrent_writers` no demuestra una sola
versión; `corruption.rs::random_byte_flips_never_panic_or_corrupt_silently`
ya compara con un prefijo del modelo, pero acepta cualquier `Err` de apertura
(una regresión hacia "rechazar todo" pasaría) y no fuzzea índices ni blobs.
`basic.rs::second_process_is_locked_out` abre dos veces en el **mismo**
proceso.

## Defensas verificadas

- WAL: preflight de la cadena completa antes de tocar archivos canónicos;
  truncado sólo del último registro incompleto sin sucesor válido; versiones
  no monótonas tratadas como corrupción; rollback de anexos parciales con
  envenenamiento del escritor si el rollback falla; continuidad de versiones
  en el replay; `required_wal_id` durable en ambos manifests antes de cambiar
  de escritor en el checkpoint en segundo plano.
- Manifest: CRC, `format_version`, validación de catálogo embebido; temporal +
  rename + fsync de directorio; `heal` nunca rota el primario corrupto sobre el
  `manifest.prev` bueno; el fallback se refresca a copia idéntica antes de
  reportar éxito; un fallo de sync de directorio valla las escrituras
  (`CommitUnknown`) hasta reabrir.
- Segmentos: CRC por entrada, longitud contra manifest, cualquier discrepancia
  rechaza la apertura escribible; los WAL obsoletos se eliminan sólo tras
  publicación canónica durable; `cleanup_orphans` respeta `manifest.prev`.
- Blobs: escritos, sincronizados y publicados antes del WAL; guardia de
  publicaciones pendientes contra el GC de compactación; GC conserva chunks
  visibles a snapshots; lectura validada (magic, longitud, CRC).
- Commit: todas las lecturas falibles ocurren antes del punto de durabilidad;
  error de anexo WAL retorna antes de mutar memoria; identidades persisten en
  el mismo registro WAL y se fusionan con máximo; comparación de esquema
  (incluido `epoch`) convierte DDL intercalado en `Conflict`; `into_ordered`
  garantiza el orden de ids que exige el atajo de `record_high_id`.
- Índices derivados: formato V3 con CRC de navegación que cubre cabeceras de
  página, límites y directorio; V1/V2 verificados contra entradas
  checksumeadas; CRC de payload propagado como `Err(Corrupt)`, nunca como
  "vacío"; generaciones exactas para primario, secundario y texto; runs
  vectoriales verificados por CRC de cuerpo y definición; una cola WAL
  truncada baja `committed_version` y por tanto invalida los runs publicados a
  la versión perdida.
- SQL: I01–I04 y H01–H02 corregidos como se describe en la tabla de
  verificación; parámetros ligados nunca interpolados; lexer ASCII sin
  inyección de identificadores; aritmética entera con comprobación de
  desbordamiento; `check_value` aplicado en INSERT y UPDATE.
- Herramientas: `backup`/`restore`/`repair` escriben en un hermano `.partial`
  privado, verifican antes de publicar y rechazan destinos existentes;
  `restore` mantiene lock compartido sobre el origen durante la copia;
  `check` reconstruye una imagen canónica independiente y valida unicidad,
  FK, identidades y correspondencia con la vista primaria/secundaria.
- Sidecar/FFI: transacciones por conexión sin ids adivinables; auth en tiempo
  constante y arranque rechazado sin token en TCP; pánicos capturados en FFI;
  handles de transacción consumidos exactamente una vez; enteros fuera de 2^53
  etiquetados como decimal en ambas direcciones.

## Orden recomendado

1. **R01** (P0): reconstrucción de índices en `finish_ddl_recovery` y fallo
   explícito ante índice declarado pero ausente. Regresión con `kill -9` en
   cada intención DDL.
2. **R02, R03, R13**: política de cola con ceros, vallado tras `fsync`
   fallido, temporizador de `Balanced`, sync en cierre, `F_FULLFSYNC`.
   Acompañar de un arnés de pérdida de energía (descartar bytes no
   sincronizados y rellenar con ceros la última extensión) que hoy no existe.
3. **R04**: `floor = min(...)` en `load_vector_run_set` y oráculo de recall en
   `vector_crash.rs`.
4. **R05, R06, R11**: ids de petición y cierre por timeout en el cliente
   Python; documentación de códigos de error y reintentos en ambos bindings;
   correcciones de fidelidad en Node.
5. **R07, R08, R09, R16**: SQL y DDL.
6. **R10, R12**: herramientas y despliegue; rotar la credencial expuesta de
   inmediato, independientemente del resto.
7. **R14, R15, R17, R18, R19**: endurecimiento y cobertura.

## Verificación ejecutada

- Lectura completa de los módulos listados en el alcance y de los tres
  documentos de revisión anteriores; se comprobó en código cada corrección
  declarada en `docs/implementacion-plan.md`.
- Tres reproducciones sobre bases temporales (R01, R07, R08) con programas de
  scratch contra el árbol actual; ninguna tocó archivos del repositorio.
- `cargo test -p elitesql-core --no-run` compila. No se ejecutó la suite
  completa ni benchmarks; los resultados citados de pruebas existentes
  provienen de la lectura de sus aserciones.
- No se simuló pérdida de energía ni fallos de disco reales; R02, R03 y R13
  se fundamentan en el código y en la semántica documentada de `fsync` y de
  los sistemas de archivos, no en observación directa.

Este documento complementa la [auditoría inicial](auditoria-y-plan-2026-09-10.md),
la [segunda revisión](revision-integridad-2026-09-10.md) y el
[informe de implementación](implementacion-plan.md). La implementación
permanece sin cambios.
