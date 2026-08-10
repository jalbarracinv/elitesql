# EliteSQL: guía autónoma para agentes

Este archivo explica cómo integrar y operar EliteSQL sin depender de que el
agente tenga acceso al repositorio ni a documentación adicional. EliteSQL es
una base de datos operativa embebida: una base es un directorio local
autocontenido, no un servicio que se deba configurar para el caso normal.

EliteSQL está en fase alfa: no asumas estabilidad de API ni de formato entre
versiones. Conserva el binario/binding y los datos bajo una política de
versionado compatible, realiza backups antes de actualizar y valida una copia
antes de actualizar una base de producción.

## Decisión inicial: quién posee la base

Elige exactamente uno de estos modelos antes de escribir código:

| Situación | Modelo | Cómo se abre |
|---|---|---|
| Un proceso de la aplicación | Embebido | Rust: `Db::open_or_create(path)`; Python: `EliteSQL(path)` |
| Varios procesos en el mismo host (Gunicorn, PHP-FPM, workers) | Sidecar mediante socket Unix | Un proceso ejecuta `elitesql serve <db> <socket>`; los workers usan `SidecarClient` |
| Aplicación y base en hosts distintos | Sidecar TCP | `elitesql serve <db> --tcp <host:puerto>` y un token obligatorio |

Una ruta como `data/app.esql` representa un **directorio** que contiene toda la
base. No lo pongas en NFS/SMB, no lo abras desde dos máquinas y no modifiques
los archivos de dentro. Para acceder desde varios procesos o equipos usa el
sidecar; un proceso es el propietario del directorio.

## CLI: creación, consulta y mantenimiento

Asume que `elitesql` está en `PATH`. Si no lo está, usa la ruta absoluta del
binario. La apertura normal no crea bases por accidente: `--create` se usa una
sola vez.

```bash
# Crear la base y ejecutar una sentencia SQL
elitesql --create query data/app.esql \
  "CREATE TABLE tasks (title text NOT NULL, done bool NOT NULL DEFAULT FALSE)"

# Ejecutar una sentencia; todas las siguientes abren la base existente
elitesql query data/app.esql "SELECT id, title FROM tasks WHERE done = FALSE"

# Shell interactivo: termina cada sentencia SQL con ;
elitesql repl data/app.esql

# Inspección y operaciones de mantenimiento
elitesql tables data/app.esql            # esquemas en JSON
elitesql check data/app.esql             # comprobación de integridad offline
elitesql compact data/app.esql           # normalmente no hace falta: hay compactación automática
elitesql backup data/app.esql backups/app.esql
elitesql restore backups/app.esql restored/app.esql
elitesql export data/app.esql tasks > tasks.jsonl
elitesql import data/app.esql tasks < tasks.jsonl
elitesql repair damaged.esql rescued.esql  # siempre escribe un destino NUEVO
elitesql version
```

Opciones globales disponibles para `query`, `repl`, `import` y `serve`:

```text
--durability safe|balanced|fast    predeterminado: safe
--read-only                        abre sin escribir ni reparar archivos; cualquier escritura falla
--create                           crea el directorio si aún no existe
```

El servidor admite además `--max-connections <n>` (128 por defecto). En TCP,
`--token-file <ruta>` o la variable `ELITESQL_TOKEN` son obligatorios; nunca
incluyas un token como argumento visible en `ps`.

Antes de una reparación, haz una copia de la base dañada. Para diagnosticar,
ejecuta primero `check`; para inspección no destructiva abre con `--read-only`
cuando corresponda. `backup` crea una copia consistente y verificada incluso
si la base se está usando.

## SQL que el agente puede usar

EliteSQL implementa un dialecto SQL deliberadamente pequeño. No supongas que
una característica de PostgreSQL, SQLite o MySQL existe si no aparece abajo.
Cada llamada a `query` contiene **una sola sentencia**.

### Tipos y tabla

Tipos disponibles: `bool`, `int` (entero firmado de 64 bits), `float64`,
`text`, `varchar(N)`, `longtext`, `enum(...)`, `blob`, `timestamp`, `date`,
`time`, `json` y `vector(N)`.

```sql
CREATE TABLE documents (
  document_id int AUTO_INCREMENT PRIMARY KEY,
  title       text NOT NULL,
  body        longtext NOT NULL,
  workspace   varchar(100) NOT NULL,
  owner_id    int REFERENCES users(user_id) ON DELETE CASCADE,
  published   bool NOT NULL DEFAULT FALSE,
  priority    int DEFAULT 0,
  metadata    json,
  created_at  timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  embedding   vector(768)
)
```

Toda tabla tiene un ULID físico interno. Si no se declara `id`, SQL lo expone
como `id text` implícito; si se declara, `id` es una columna SQL ordinaria y
puede ser `int AUTO_INCREMENT PRIMARY KEY`. El máximo de la identidad se
conserva en WAL/manifiesto y avanza al importar un valor explícito mayor.

Las columnas son anulables por defecto. Usa `NOT NULL` y `DEFAULT <literal>`
para imponer datos. Las referencias de una columna admiten `ON DELETE
RESTRICT` (predeterminado) y `CASCADE`, y deben apuntar al `id` o a una columna
con índice único. La validación y la cascada forman parte del mismo commit.
No existen claves foráneas compuestas ni `ALTER TABLE ... ADD FOREIGN KEY`,
`CHECK`, `UNIQUE` en línea o índices compuestos; expresa unicidad con
`CREATE UNIQUE INDEX`. Las autorreferencias y sus ciclos de cascada sí se
resuelven de forma finita.

Los literales SQL importantes son:

```sql
TRUE, FALSE, NULL, 42, 3.14, 'texto con ''comilla''', X'DEADBEEF'
'2026-08-07'                         -- date
'09:30:00'                            -- time
'2026-08-07 09:30:00'                 -- timestamp UTC
'{"source":"web","tags":["a"]}'  -- json: texto que contiene JSON válido
'[0.12, -0.04, 0.87]'                 -- vector: texto que contiene un array JSON
```

Un vector debe tener exactamente `N` números; por ejemplo, un valor para
`vector(768)` debe tener 768 componentes. Los timestamps representan instantes
UTC; no se admiten offsets como `+05:00`.

### Gramática práctica

Usa estas formas; las palabras clave no distinguen mayúsculas/minúsculas y se
admiten comentarios `-- línea` y `/* bloque */`:

```text
CREATE TABLE tabla (
  col tipo [AUTO_INCREMENT|GENERATED BY DEFAULT AS IDENTITY]
    [PRIMARY KEY] [NOT NULL] [DEFAULT literal]
    [REFERENCES padre(col) [ON DELETE RESTRICT|CASCADE]], ...)
CREATE [UNIQUE] INDEX [nombre] ON tabla (columna)
ALTER TABLE tabla ADD [COLUMN] columna tipo [NOT NULL] [DEFAULT literal]
ALTER TABLE tabla DROP [COLUMN] [IF EXISTS] columna
ALTER TABLE tabla RENAME [COLUMN] columna TO nuevo_nombre
ALTER TABLE tabla RENAME TO nuevo_nombre
DROP TABLE [IF EXISTS] tabla
DROP INDEX [IF EXISTS] [nombre] ON tabla (columna)

INSERT [IGNORE] INTO tabla [(columna, ...)]
  VALUES (literal_o_parámetro, ...), (...)
  [ON CONFLICT DO NOTHING] [RETURNING columna, ...]
SELECT proyección FROM tabla [alias]
  [JOIN/LEFT JOIN/RIGHT JOIN tabla [alias] ON alias.col = alias.col]
  [WHERE predicado] [GROUP BY columna, ...] [HAVING predicado]
  [ORDER BY columna [COLLATE unicode|binary] [ASC|DESC], ...]
  [LIMIT entero] [OFFSET entero]
EXPLAIN SELECT ...
UPDATE tabla SET columna = literal_o_parámetro
  | columna_aritmética [+|-|*|/] número [, ...] [WHERE predicado]
DELETE FROM tabla [WHERE predicado]
```

`id` se puede seleccionar, filtrar e insertar explícitamente; al omitir la
lista de columnas de `INSERT`, los valores siguen el orden de columnas
declaradas y **no** incluyen el `id` implícito. Inserciones múltiples son
atómicas y devuelven los ids; `UPDATE`/`DELETE` devuelven el número afectado.

### Índices y cambios de esquema

```sql
CREATE INDEX ON documents (workspace)
CREATE UNIQUE INDEX ON users (email)
ALTER TABLE documents ADD COLUMN language text
ALTER TABLE documents ADD COLUMN visibility text NOT NULL DEFAULT 'private'
ALTER TABLE documents RENAME COLUMN title TO heading
ALTER TABLE documents DROP COLUMN IF EXISTS language
DROP INDEX ON documents (workspace)
DROP TABLE IF EXISTS obsolete_table
```

Los índices escalares son de una columna y el planificador los emplea
automáticamente para igualdades y joins. `NULL` no participa en una unicidad:
un índice único permite varios `NULL`.

`ADD COLUMN` sin `DEFAULT` es inmediato; con `DEFAULT` recorre y rellena los
registros existentes. Los cambios DDL no son transaccionales ni versionados por
snapshot: programa migraciones cuando no haya código que dependa del esquema
anterior. Gestiona qué migraciones se aplicaron en el proyecto; no dependas de
`CREATE TABLE IF NOT EXISTS` ni de `CREATE INDEX IF NOT EXISTS`.

### CRUD, filtros y agregados

```sql
INSERT INTO documents (title, body, workspace, published)
VALUES ('Guía', 'Contenido', 'acme', FALSE)
RETURNING document_id, id

-- Inserción múltiple: un único commit atómico
INSERT INTO tasks (title, done) VALUES ('uno', FALSE), ('dos', FALSE)

SELECT id, title, priority
FROM documents
WHERE workspace = 'acme' AND published = TRUE
ORDER BY priority DESC, title ASC
LIMIT 20 OFFSET 0

SELECT title FROM documents WHERE priority IN (1, 2, 3)
SELECT title FROM documents WHERE metadata IS NULL

UPDATE documents SET published = TRUE WHERE id = 'doc-1'
UPDATE accounts SET credits = credits - 1
WHERE user_id = 7 AND credits >= 1
DELETE FROM documents WHERE workspace = 'obsolete'

SELECT workspace, count(*) AS total, avg(priority) AS average_priority
FROM documents
WHERE published = TRUE
GROUP BY workspace
HAVING count(*) >= 5
ORDER BY total DESC
LIMIT 10
```

En `WHERE` se admiten `=`, `!=`/`<>`, `<`, `<=`, `>`, `>=`, `AND`, `OR`,
`NOT`, paréntesis, `IS NULL`, `IS NOT NULL`, `IN` y `NOT IN`. Usa `IS NULL`,
nunca `= NULL`. Las comparaciones con `NULL` son desconocidas y no pasan el
filtro. `UPDATE SET` acepta literales/parámetros y aritmética de la propia
columna numérica (`+`, `-`, `*`, `/`), con overflow y división por cero
comprobados. Otras expresiones se calculan en la aplicación.

Funciones de agregación: `COUNT(*)`, `COUNT(col)`, `COUNT(DISTINCT col)`,
`SUM(col)`, `AVG(col)`, `MIN(col)` y `MAX(col)`. La forma global de
`COUNT(DISTINCT col)` ignora `NULL` y derrama bajo el presupuesto de memoria.
Toda columna seleccionada que no sea agregada debe estar en `GROUP BY`. No hay
`INSERT ... SELECT`, `UPDATE/DELETE ... RETURNING`, subconsultas, ventanas ni
sentencias SQL desnudas `BEGIN`/`COMMIT`; las transacciones se abren mediante
la API.

`INSERT IGNORE` y `ON CONFLICT DO NOTHING` sólo omiten conflictos de `id` o de
índice único. No ocultan errores de tipo, esquema o clave foránea. `INSERT ...
RETURNING` devuelve las columnas solicitadas y es la forma recomendada de
obtener una identidad generada.

### Joins, ordenación y plan de consulta

Se admiten `JOIN`/`INNER JOIN`, `LEFT JOIN` y `RIGHT JOIN`. La condición `ON`
debe ser exactamente una igualdad de columnas; los filtros adicionales van en
`WHERE`. En consultas con varias tablas, califica las columnas repetidas.

```sql
CREATE INDEX ON orders (user_id)

SELECT u.name, o.amount
FROM users AS u
JOIN orders AS o ON o.user_id = u.id
WHERE u.email = 'ana@example.com' AND o.amount > 100
ORDER BY o.amount DESC
LIMIT 20

-- Los usuarios sin pedido se mantienen y las columnas de o son NULL.
SELECT u.name, o.amount
FROM users u LEFT JOIN orders o ON o.user_id = u.id

EXPLAIN SELECT u.name, o.amount
FROM users u JOIN orders o ON o.user_id = u.id
WHERE u.email = 'ana@example.com'
```

Indexa las claves de join y los campos de igualdad frecuentes. `EXPLAIN
<SELECT>` valida y muestra el plan sin ejecutarlo: busca `POINT LOOKUP`,
`INDEX LOOKUP` o `INDEX PROBE`; una línea `SCAN ... no index` señala un índice
que puede ser necesario. No hay `EXPLAIN ANALYZE`, `FULL OUTER JOIN` ni
`CROSS JOIN`.

`ORDER BY` permite varias columnas con `ASC` (predeterminado) o `DESC` y una
collation por clave: `COLLATE unicode` (predeterminada, orden alfabético para
latín) o `COLLATE binary` (bytes UTF-8). Las comparaciones de igualdad y los
índices escalares siguen siendo exactos por bytes aunque el orden unicode
agrupe mayúsculas y acentos. Los `NULL` ordenan primero. La ordenación, joins
y agregados usan
memoria acotada y derraman datos temporales a disco cuando hace falta.

### Operaciones que no existen y su alternativa

| No usar | Hacer en su lugar |
|---|---|
| `LIKE` | Crear índice de texto y usar `search_text` (BM25) |
| `BETWEEN a AND b` | `>= a AND <= b` |
| `DISTINCT`, CTE, subconsulta, `UNION` | Consultas separadas y combinación/deduplicación en la aplicación; sólo `COUNT(DISTINCT col)` global está soportado |
| Aritmética en proyecciones u otras funciones SQL | Calcular en la aplicación; `UPDATE SET col = col + n` (también `-`, `*`, `/`) sí está soportado |
| `ALTER COLUMN` / `MODIFY` | Añadir columna nueva, copiar datos, eliminar la antigua |
| `ON DUPLICATE KEY UPDATE`, `REPLACE INTO` | `INSERT IGNORE`/`ON CONFLICT DO NOTHING` si basta omitir el duplicado; si no, SELECT + INSERT/UPDATE dentro de una transacción |
| `TRUNCATE` | `DELETE FROM tabla`, o `DROP TABLE` y crearla de nuevo |
| `LIMIT offset, count` | `LIMIT count OFFSET offset` |
| `FOR UPDATE` o bloqueos de fila | Commit optimista y reintento de conflicto |
| funciones vectoriales/textuales SQL | API `search_vector`, `search_text` o `search_hybrid` |

### Parámetros: regla obligatoria de seguridad

Nunca interpoles valores de usuario en el SQL. Pasa la sentencia y los valores
por separado. Se aceptan `?` y `%s` posicionales, o `%(nombre)s` nombrados.

```python
db.query(
    "SELECT id, title FROM documents WHERE workspace = %(workspace)s LIMIT %(limit)s",
    {"workspace": workspace_id, "limit": 20},
)
db.query(
    "INSERT INTO documents (title, body, workspace, embedding) VALUES (%s, %s, %s, %s)",
    [title, body, workspace_id, embedding],
)
```

Los parámetros preservan tipos: `None`, booleanos, enteros int64, floats,
texto, bytes, fechas/horas, objetos/listas JSON y listas numéricas para
vectores. No mezcles estilos en una sentencia; los nombres y el número de
parámetros deben coincidir exactamente. `LIMIT` y `OFFSET` pueden ser
parámetros enteros no negativos.

## Búsqueda: vectorial, texto e híbrida

Primero crea una columna `vector(N)` y un índice ANN (HNSW) con la API, no con
SQL. También se crea el índice BM25 de texto con la API. Las tres operaciones
de búsqueda devuelven resultados con `id`, puntuación o `distance`, y el
registro completo.

```python
# db puede ser EliteSQL (embebido) o SidecarClient (sidecar)
db.create_vector_index(
    "documents", "embedding",
    metric="cosine",       # "cosine" (predeterminado), "dot" o "l2"
    mode="sync",           # "sync" o "async"
    quantized=True,         # índice int8: ~4x menos memoria para vectores
)
db.create_text_index("documents", "body")

# ANN: distancia menor significa más parecido.
hits = db.search_vector(
    "documents", "embedding", query_embedding,
    top_k=10, ef_search=128, filter={"workspace": workspace_id},
)

# BM25 de texto.
hits = db.search_text(
    "documents", "body", user_query,
    top_k=10, filter={"workspace": workspace_id},
)

# Fusión RRF de texto y vector; se puede usar una fuente o ambas.
hits = db.search_hybrid(
    "documents",
    text=("body", user_query),
    vector=("embedding", query_embedding),
    top_k=10,
    ef_search=128,
    filter={"workspace": workspace_id},
)
```

Con métrica coseno, `distance = 1 - cosine_similarity`: `0` es idéntico y los
valores menores son mejores. `ef_search` mayor mejora el recall a costa de
latencia. `mode="async"` hace que el commit no espere la indexación: es útil
para carga masiva, pero un vector recién insertado puede no aparecer
brevemente. Usa modo síncrono para resultados inmediatamente visibles.

No intentes `ORDER BY distance(...)`, comparar vectores en `WHERE`, ni buscar
vectores desde SQL: no existe esa sintaxis. Esas tareas se realizan mediante
`search_vector` y `search_hybrid`. El filtro de búsqueda admite igualdades en
las demás columnas; úsalo para aislamiento por tenant/workspace.

### Detalles de búsqueda que afectan al producto

- El índice de texto divide por caracteres no alfanuméricos, descarta tokens de
  menos de 2 o más de 64 caracteres y los pasa a minúsculas. No ofrece
  stemming, sinónimos, frases, prefijos, operadores booleanos ni filtros de
  stopwords. Una consulta BM25 devuelve mayor `score` primero.
- El índice vectorial puede construirse antes o después de cargar datos; si se
  crea después indexa los registros ya comprometidos. Para una carga inicial
  grande, inserta y después crea los índices derivados.
- `m` controla conexiones HNSW (rango admitido 1–256; 12–48 suele ser el
  rango práctico) y `ef_construction` el coste/calidad de construcción. La
  métrica, cuantización y estos parámetros quedan fijados al crear el índice:
  para cambiarlos, elimina y recrea el índice con la API Rust.
- Los índices vectoriales, de texto y escalares se actualizan al insertar,
  actualizar o borrar registros. Son estructuras derivadas: se pueden
  reconstruir a partir de los datos canónicos; nunca son la única copia de los
  datos.
- Para eliminar índices de texto o vector se usa la API (`drop_text_index` o
  `drop_vector_index` en Rust); SQL sólo elimina los índices escalares.
- En Rust, tras una carga con `IndexingMode::Async`, llama a
  `db.wait_vector_indexing()` antes de atender búsquedas que deban incluirla.

## Lecturas grandes, cargas y recursos

`Db::query()` materializa todas las filas del resultado en memoria del
llamador. Para resultados grandes en Rust usa `query_cursor`,
`query_cursor_params` o `query_cursor_named_params`; entregan filas por lotes
y actualmente cubren `SELECT` de una sola tabla con `WHERE`, proyección,
`LIMIT` y `OFFSET`.

```rust
let mut cursor = db.query_cursor("SELECT id, title FROM documents LIMIT 100000")?;
while let Some(row) = cursor.next() {
    let row = row?;
    // procesar y descartar row
}
```

El motor limita su propia memoria por base: por defecto el presupuesto total
es 384 MiB, repartido entre consultas, deltas de índices y mantenimiento. Si
una consulta, un `top_k`, una transacción o la construcción de un índice no
cabe, devuelve `Error::MemoryLimit`; divide el trabajo en lotes o aumenta los
presupuestos con `DbOptions`. Para ingestión sostenida Rust ofrece
`DbOptions::ingest_performance()` (perfil opt-in de 512 MiB). No aumentes la
memoria sin considerar los límites reales del proceso y del contenedor.

Para una importación inicial muy especializada, la API Rust
`bulk_insert_sorted("tabla", records)` acepta registros con `id text` no vacío
en orden estrictamente ascendente. Sólo úsala si no existen índices escalares,
de texto o vector en esa tabla; crea los índices después. Para importaciones
normales, desordenadas o incrementales usa `INSERT` o un `Txn`.

La compactación es automática. `checkpoint`, DDL, cierre y `compact` son
barreras de mantenimiento; no las ejecutes por cada escritura. Llama a
`compact` manualmente sólo si necesitas recuperar espacio físico ahora o tras
una operación masiva de borrado/reescritura.

## Integración por lenguaje

### Python embebido

El binding requiere `libelitesql` disponible: configúralo mediante
`ELITESQL_LIB=/ruta/a/libelitesql.{so,dylib}` si no se detecta solo.

```python
from elitesql import EliteSQL

with EliteSQL("data/app.esql") as db:
    db.query("CREATE TABLE tasks (task_id int AUTO_INCREMENT PRIMARY KEY, "
             "title text NOT NULL, done bool NOT NULL DEFAULT FALSE)")
    cursor = db.cursor().execute(
        "INSERT INTO tasks (title) VALUES (%s)", ["Preparar lanzamiento"]
    )
    generated = cursor.lastrowid
    rows = db.cursor().execute(
        "SELECT task_id, title FROM tasks WHERE done = %s", [False]
    ).fetchall()
```

El binding libera el GIL durante cada llamada al motor, por lo que los threads
pueden ejecutar operaciones reales en paralelo. Usa `with db.snapshot() as s:`
con `s.get(tabla, id)` o `s.scan(tabla)` para una vista estable mientras otros
writers continúan. El cursor compatible con DB-API ofrece `execute`,
`executemany`, `fetchone`, `fetchmany`, `fetchall`, `rowcount`, `description` y
`lastrowid`; este último es la primera identidad generada, o el `id` físico si
la tabla no declara identidad.

Para atomicidad multi-sentencia usa `with db.transaction() as tx:` y ejecuta
`tx.query(...)`/`tx.cursor()`. `db.run_transaction(callback, retries=3)` puede
reintentar automáticamente `CONFLICT_RETRY`, pero sólo debe usarse si el
callback puede volver a ejecutarse y no envía correos, webhooks ni otros efectos
externos.

### Python sidecar

Un solo proceso inicia el servidor y posee la base:

```bash
elitesql serve data/app.esql /tmp/elitesql.sock
```

Cada worker se conecta al socket:

```python
from elitesql import SidecarClient

db = SidecarClient("/tmp/elitesql.sock")
result = db.query("SELECT count(*) AS total FROM tasks")

with db.transaction() as tx:
    tx.query("UPDATE accounts SET credits = credits - 1 "
             "WHERE user_id = %s AND credits >= 1", [user_id])
    tx.query("INSERT INTO documents (owner_id, title) VALUES (%s, %s)",
             [user_id, title])
```

Para TCP, el token nunca se pasa como argumento de línea de comandos:

```bash
export ELITESQL_TOKEN="secreto-aleatorio-largo"
elitesql serve data/app.esql --tcp 127.0.0.1:7070
```

```python
import os

db = SidecarClient(host="db-host", port=7070, token=os.environ["ELITESQL_TOKEN"])
```

TCP no cifra el tráfico. Enlaza a loopback y usa túnel SSH, VPN o una red
privada. Las transacciones del sidecar están ligadas a una conexión y usan las
operaciones `begin`, `query_in_txn`, `commit` y `rollback`, no sentencias SQL
desnudas. Un disconnect o el plazo de 30 segundos provoca rollback.

### Node.js: sidecar

El cliente Node sólo funciona contra el sidecar y requiere Node 18 o posterior.

```js
const { SidecarClient } = require('@elitesql/client');

const db = await SidecarClient.connect('/tmp/elitesql.sock');
const result = await db.query(
  'SELECT id, title FROM tasks WHERE done = %s LIMIT %s',
  [false, 20],
);
const hits = await db.searchVector('documents', 'embedding', embedding, {
  topK: 10,
  filter: { workspace: workspaceId },
});
db.close();
```

Las operaciones equivalentes son `createVectorIndex`, `createTextIndex`,
`searchText` y `searchHybrid`. Para TCP, conecta con
`SidecarClient.connect({ host, port, token })`.

### Rust embebido

```rust
use elitesql_core::{Db, QueryOutput, Value};

let db = Db::open_or_create("data/app.esql")?;
db.query_params(
    "INSERT INTO tasks (title, done) VALUES (%s, %s)",
    &[
        Value::Text("Preparar lanzamiento".into()),
        Value::Bool(false),
    ],
)?;
if let QueryOutput::Rows { columns, rows } = db.query("SELECT id, title FROM tasks")? {
    // consumir columns y rows
}
```

Para varias escrituras que deban ser atómicas, usa `let mut txn = db.begin()`;
ejecuta SQL con `txn.query`/`txn.query_params` o las operaciones estructuradas y
llama a `txn.commit()`. Si devuelve `Error::Conflict`, vuelve a ejecutar el
bloque completo con un número limitado de reintentos. No hay sentencias SQL
desnudas `BEGIN`/`COMMIT`; la misma transacción estructurada está disponible en
la ABI C, Python embebido y sidecar.

La API Rust directa también expone `insert`, `update`, `delete`, `get`,
`scan`, `scan_batch`, `find_eq`, `find_eq_batch`, `snapshot`, `checkpoint`,
`compact`, `backup`, `tables`, `table_schema`, `maintenance_stats`,
`query_memory_stats` y `global_memory_stats`. Para esquema, expone
`create_table`, `create_index`, `add_column`, `drop_column`, `rename_column`,
`rename_table`, `drop_table`, `drop_index`, `create/drop_text_index` y
`create/drop_vector_index`.

Ejemplo de transacción explícita con reintento (el contenido de `Record` usa
`BTreeMap<String, Value>`):

```rust
use elitesql_core::{Error, Record, Value};

for _attempt in 0..3 {
    let mut txn = db.begin();
    let mut task = Record::new();
    task.insert("title".into(), Value::Text("Preparar lanzamiento".into()));
    let id = txn.insert("tasks", task)?;
    let mut patch = Record::new();
    patch.insert("done".into(), Value::Bool(true));
    txn.update("tasks", &id, patch)?;
    match txn.commit() {
        Ok(_) => break,
        Err(Error::Conflict(_)) if _attempt < 2 => continue,
        Err(error) => return Err(error.into()),
    }
}
```

### C ABI

Si el proyecto integra la biblioteca C, el handle es thread-safe. Abre con
`elitesql_open(path, options_json, &db)` y cierra con `elitesql_close(db)`.
Las llamadas devuelven `uint32_t`: `0` es éxito; los códigos estables son
`1` I/O, `2` corrupción, `3` tabla existente, `4` tabla ausente, `5` registro
ausente, `6` id duplicado, `7` violación de esquema, `8` argumento inválido,
`9` conflicto reintentable, `10` base bloqueada, `11` unicidad, `12` SQL,
`13` sólo lectura, `14` columna ausente, `15` índice ausente, `16` límite de
memoria y `100` pánico interno. En un error, lee `elitesql_last_error()` (no se
libera). Las salidas JSON asignadas por la biblioteca se liberan exactamente
una vez con `elitesql_free_string()`.

La ABI C ofrece `elitesql_query`, `elitesql_query_params`, transacciones
estructuradas (`elitesql_txn_begin`, `elitesql_txn_query_params`, CRUD,
`elitesql_txn_commit`/`rollback`/`close`), búsqueda/creación de índices
vectoriales y de texto, búsqueda híbrida, snapshots, `checkpoint`, `compact`,
`check` y `repair`. Los resultados de una consulta son uno de:

```json
{"columns":["id","title"],"rows":[["01...","Guía"]]}
{"inserted":["01..."],"lastrowid":7}
{"affected":1}
{"ok":true}
```

`lastrowid` es la primera identidad generada por el statement, o el primer
`id` físico cuando no hay identidad. `INSERT ... RETURNING` produce el formato
`columns`/`rows`. Una transacción devuelve código `9` al detectar conflicto;
el llamador debe cerrarla y repetir la unidad completa.

Los valores no JSON nativos se codifican con etiquetas; por ejemplo
`{"$t":"blob","hex":"DEADBEEF"}`, `{"$t":"vector","v":[0.1,0.2]}`,
`{"$t":"timestamp","us":1775556600000000}` y, para entradas int64 sin
pérdida, `{"$t":"int64","v":"9223372036854775807"}`.

## Ciclo de vida, backups y recuperación

Cada commit pasa por un WAL con checksum y se publica de forma atómica. Tras
un crash, EliteSQL carga el manifiesto válido (con copia anterior), reproduce
el WAL de forma idempotente y descarta una cola incompleta. El resultado es el
último commit completo: nunca aparece medio commit. Los índices derivados se
recuperan o reconstruyen desde los datos canónicos.

La política operativa es:

1. Crear backups regulares **antes** de que haya un incidente. Desde Rust,
   `db.backup(destino)` puede ejecutarse mientras otros threads escriben. El
   comando CLI necesita el lock de la base, por lo que se usa con el proceso
   propietario detenido o en una ventana de mantenimiento.
2. Restaurar sólo a una ruta que no exista: `elitesql restore <backup> <dst>`
   valida el backup antes de materializarlo.
3. Para una copia manual, sólo es válido copiar recursivamente una base que
   está cerrada. Nunca copies archivo a archivo una base abierta a escritura:
   la copia puede mezclar archivos de instantes distintos.
4. Si hay señales de corrupción, ejecuta `elitesql check <db>`, que no modifica
   nada. Si la apertura normal falla, exporta lo legible con
   `elitesql export <db> <tabla> --read-only > rescate.jsonl`.
5. Sólo entonces usa `elitesql repair <src> <dst>`. Requiere un `catalog.json`
   legible y genera un informe de registros recuperados y daños; puede perder
   datos posteriores al primer punto corrupto del archivo. Jamás sobrescribe
   el origen.

Una corrupción de `catalog.json` no se autorrepara porque contiene el esquema
necesario para interpretar registros: restáuralo desde backup. Los blobs
corruptos afectan al registro que los referencia; `check` los informa. Los
índices y sus grafos ANN son descartables, los datos no.

## Concurrencia, durabilidad y entrega

- Las consultas SQL ven el último estado comprometido (read committed). Un
  snapshot ofrece una vista coherente para varias lecturas.
- `UPDATE` y `DELETE` autocommit gestionan automáticamente reintentos de
  conflicto. En una transacción explícita Rust devuelve `Error::Conflict` y
  las APIs C/Python el código `9`; el llamador debe repetir la unidad completa.
- Las identidades y claves foráneas se validan durante el commit optimista. No
  uses el entero generado como reloj global ni asumas que no habrá huecos tras
  abortos/conflictos: la monotonía, no la contigüidad, es la garantía.
- Si un proceso obtiene `DatabaseLocked`/código `10`, no intentes abrir el
  mismo directorio en otro proceso: cambia el despliegue a sidecar. En Python
  el código de conflicto es `EliteSQLError.CONFLICT_RETRY` (`9`).
- Trata `UniqueViolation` como conflicto de datos del usuario, `Sql` o
  `SchemaViolation` como error de implementación/migración y `MemoryLimit`
  como señal para paginar, reducir `top_k`, usar cursor o aumentar el
  presupuesto explícitamente. No reintentes ciegamente esos errores.
- El modo de durabilidad predeterminado es `safe` y hace `fsync` por commit.
  `balanced` puede perder los últimos milisegundos tras un crash del sistema;
  `fast` puede perder cambios recientes. No elijas uno menos seguro sin que el
  requisito de producto lo permita explícitamente.
- Evita patrones N+1 cuando uses TCP: una consulta local puede durar
  microsegundos, pero un viaje de red cuesta milisegundos.
- Programa backups y prueba restaurarlos. Ejecuta migraciones y pruebas de
  integración contra un directorio de base nuevo antes de entregar.

### Evidencia de rendimiento vigente

La repetición del 2026-08-09 en Apple M5, después de añadir compatibilidad
relacional, cargó 10 millones de filas con el perfil histórico de 128 MiB en
24.049 s frente a 15.670 s de SQLite (1.535x su tiempo). Con 1/2/4/8 writers,
el throughput de EliteSQL cambió como máximo 2% respecto a la medición anterior
y fue 1.86x–2.40x el de SQLite. El p99 empeoró 22.1% con ocho writers y hubo
picos aislados de 4–8 ms, por lo que la cola sigue siendo trabajo pendiente.

No generalices ese resultado a todas las nuevas funciones: los fixtures usan
`id text` explícitos y no declaran identidades ni referencias. Validan que la
ruta transaccional común no sufrió una regresión material, pero todavía hace
falta un microbenchmark dedicado a asignación de identidades y validación/
cascada de claves foráneas. La metodología y CSV están en `benchmark.md`.

## Lista de comprobación para cambios

1. Confirmar si el proceso único usa embedded o si varios procesos necesitan
   sidecar.
2. Mantener la base en almacenamiento local persistente y fuera de NFS/SMB.
3. Crear o migrar el esquema explícitamente; añadir índices escalares, de
   texto o vectoriales sólo para rutas de consulta reales.
4. Si se importan identidades enteras, cargarlas explícitamente y comprobar el
   siguiente valor generado; declarar referencias sólo después de validar que
   no hay huérfanos.
5. Encapsular cada operación multi-sentencia en una transacción con reintento
   acotado y separar de ella los efectos externos no repetibles.
6. Usar siempre parámetros para datos externos, incluidos límites y vectores.
7. Elegir `search_vector`, `search_text` o `search_hybrid` en vez de inventar
   funciones SQL de búsqueda.
8. Probar el ciclo real: creación/migración, escritura, consulta, filtros de
   tenant, búsqueda si aplica, reinicio y copia de seguridad/restauración.
