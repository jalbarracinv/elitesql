# Manual SQL de ClawDB

Referencia del dialecto SQL de ClawDB (V1). Es un subset deliberadamente pequeno: cubre lo que una app operacional necesita y rechaza todo lo demas con un error explicito que dice que usar en su lugar.

## Como ejecutar SQL

```rust
use clawdb_core::{Db, QueryOutput};

let db = Db::open_or_create("app.clawdb")?;
match db.query("SELECT name FROM users WHERE age > 30")? {
    QueryOutput::Rows { columns, rows } => { /* SELECT */ }
    QueryOutput::Inserted { ids }        => { /* INSERT: ids generados */ }
    QueryOutput::Affected(n)             => { /* UPDATE/DELETE: filas afectadas */ }
    QueryOutput::None                    => { /* DDL */ }
}
```

Reglas generales:

- Una sentencia por llamada a `query()`.
- Keywords case-insensitive (`SELECT` = `select`).
- Comentarios de linea con `--`.
- Los SELECT leen el ultimo estado commiteado (read committed). Para lecturas consistentes por snapshot usa la API Rust (`db.snapshot()` + `scan_at`/`get_at`).
- UPDATE/DELETE se aplican en una transaccion con reintentos automaticos ante conflicto optimista. Un INSERT multi-fila es un solo commit atomico: o entran todas las filas o ninguna.

## Tipos y literales

| Tipo | Literal SQL | Ejemplo |
|---|---|---|
| `bool` | `TRUE` / `FALSE` | `TRUE` |
| `int64` | entero | `42`, `-7` |
| `float64` | decimal o entero | `3.14`, `3` |
| `text` | string entre comillas simples | `'hola'`, `'it''s ok'` (escape `''`) |
| `blob` | hex literal | `X'DEADBEEF'` |
| `timestamp` | string `'YYYY-MM-DD HH:MM:SS[.ffffff]'` (UTC) o entero (microsegundos Unix) | `'2026-08-07 09:30:00'` |
| `date` | string `'YYYY-MM-DD'` (dias desde epoch por dentro) | `'2026-08-07'` |
| `time` | string `'HH:MM:SS[.ffffff]'` (microsegundos desde medianoche) | `'09:30:00'` |
| `json` | string con JSON valido | `'{"tags": ["a"], "n": 3}'` |
| `vector(N)` | string con array JSON de N numeros | `'[0.12, -0.5, 0.33]'` |
| null | `NULL` | `NULL` |

Los literales de `date`, `time` y `timestamp` se validan de verdad: `'2026-02-30'` o `'25:00:00'` fallan con error claro. En WHERE, un string se coerciona automaticamente contra columnas date/time/timestamp: `WHERE day >= '2026-01-01'` o `WHERE at < '2026-08-07 18:00:00'` funcionan sin sintaxis de cast (tambien contra indices).

Sobre `timestamp` (el "datetime" de ClawDB): representa un instante en microsegundos UTC. El literal acepta separador espacio o `T`, sufijo `Z` opcional, y fecha sola (`'2026-08-08'` = medianoche UTC). No hay offsets de timezone (`+05:00`): el motor guarda instantes; la presentacion por zona horaria es de la aplicacion.

**Diferencias de fechas**: se calculan en la aplicacion, y la representacion las hace triviales — `date` son dias desde epoch, asi que restar dos `Value::Date` da la diferencia en dias directamente (los bisiestos ya los resolvio el motor al parsear); `time` y `timestamp` restan en microsegundos. Dentro de una query, expresa "ultimos N dias" o "entre fechas" como rangos, que ademas usan indices:

```sql
SELECT * FROM pedidos WHERE fecha >= '2026-07-08'                      -- limite calculado por la app
SELECT * FROM eventos WHERE dia >= '2026-08-01' AND dia < '2026-09-01' -- rango indexable
```

Aritmetica dentro del SQL (`fecha2 - fecha1` en proyeccion o HAVING) queda para una fase futura de expresiones minimas.

Coerciones automaticas: un entero es valido para columnas `float64` y `timestamp`. Un string es valido para `json` solo si parsea como JSON.

## La primary key implicita

Toda tabla tiene una columna `id` de tipo `text` que **no se declara**. Si no la provees en el INSERT, el motor genera un [ULID](https://github.com/ulid/spec) (26 caracteres, ordenable por tiempo). Puedes proveerla explicitamente, y no puede modificarse con UPDATE.

## CREATE TABLE

```sql
CREATE TABLE users (
  name      text NOT NULL,
  email     text,
  age       int64,
  prefs     json,
  embedding vector(768)
)
```

- Columnas nullable por defecto; `NOT NULL` para exigir valor.
- No hay `PRIMARY KEY` (es el `id` implicito), ni `DEFAULT`, ni `REFERENCES`, ni `UNIQUE` inline (usa `CREATE UNIQUE INDEX`).
- Los nombres de tipo son exactos: `int` o `varchar` fallan con un error que indica el tipo correcto (`use int64`, `use text`).

## CREATE INDEX

```sql
CREATE INDEX ON orders (user_id)          -- indice de igualdad
CREATE UNIQUE INDEX ON users (email)      -- unicidad validada en cada commit
CREATE UNIQUE INDEX idx_email ON users (email)  -- el nombre es opcional
```

- Un indice por columna; multi-columna no soportado en V1.
- Los `NULL` no participan de la unicidad (varias filas pueden tener `email NULL`).
- Crear un indice unico sobre datos con duplicados existentes falla.
- El planner los usa automaticamente en igualdades de WHERE y en JOINs.

## INSERT

```sql
INSERT INTO users (name, email, age) VALUES ('ana', 'ana@x.com', 30)

-- Multi-fila: un solo commit atomico. Devuelve los ids en orden.
INSERT INTO users (name, age) VALUES ('bob', 25), ('eva', 41)

-- Con id explicito:
INSERT INTO users (id, name) VALUES ('u-admin', 'root')
```

- La lista de columnas es **obligatoria**.
- Columnas no listadas quedan `NULL` (error si son `NOT NULL`).
- Devuelve `QueryOutput::Inserted { ids }` con los ULIDs generados o los ids provistos.
- `INSERT ... SELECT` y `RETURNING` no estan soportados.

## SELECT

```sql
SELECT * FROM users
SELECT name, age FROM users
SELECT name AS who, age AS years FROM users
```

### WHERE

Operadores: `=`, `!=` (o `<>`), `<`, `<=`, `>`, `>=`, `AND`, `OR`, `NOT`, parentesis, `IS NULL`, `IS NOT NULL`, `IN (...)`, `NOT IN (...)`.

```sql
SELECT name FROM users WHERE age >= 25 AND email IS NOT NULL
SELECT name FROM users WHERE age IN (25, 30, 41)
SELECT name FROM users WHERE (age < 18 OR age > 65) AND NOT name = 'admin'
SELECT name FROM users WHERE id = 'u-admin'        -- point lookup directo
```

Semantica de NULL (logica simplificada de dos valores): cualquier comparacion que involucre `NULL` es falsa. `email = NULL` nunca matchea — usa `email IS NULL`.

### ORDER BY, LIMIT, OFFSET

```sql
SELECT name, age FROM users ORDER BY age DESC, name ASC LIMIT 10 OFFSET 20
```

Los `NULL` ordenan primero. `LIMIT`/`OFFSET` se aplican despues de ordenar.

## JOINs

Soportados: `INNER JOIN` (o `JOIN`), `LEFT JOIN`, `RIGHT JOIN`. La condicion `ON` es exactamente una igualdad de columnas; filtros adicionales van en `WHERE`.

```sql
-- Ordenes de un usuario, usando el indice de orders.user_id:
SELECT u.name, o.amount
FROM users u
INNER JOIN orders o ON o.user_id = u.id
WHERE u.email = 'ana@x.com'
ORDER BY o.amount DESC

-- LEFT JOIN: usuarios sin ordenes aparecen con NULL:
SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON o.user_id = u.id

-- Joins encadenados:
SELECT u.name, o.amount, t.tag
FROM users u
JOIN orders o ON o.user_id = u.id
JOIN tags t   ON t.order_id = o.id
```

- Alias de tabla con o sin `AS` (`users u` o `users AS u`).
- Con mas de una tabla, las columnas repetidas (como `id`) deben calificarse: `u.id`. `SELECT *` devuelve headers calificados (`u.id`, `o.amount`).
- `RIGHT JOIN` preserva la tabla derecha (internamente es un LEFT con roles invertidos).
- `FULL OUTER JOIN` y `CROSS JOIN` no estan soportados.

## Agregados, GROUP BY y HAVING

Funciones: `COUNT(*)`, `COUNT(col)`, `SUM(col)`, `AVG(col)`, `MIN(col)`, `MAX(col)`.

```sql
-- Agregado global: siempre devuelve exactamente una fila.
SELECT count(*), sum(amount), avg(amount) FROM sales

-- Por grupo, con filtro de grupos y orden por alias:
SELECT region, count(*) AS n, sum(amount) AS total
FROM sales
WHERE amount > 0
GROUP BY region
HAVING sum(amount) >= 300
ORDER BY total DESC
LIMIT 10

-- Compone con joins:
SELECT g.country, sum(s.amount) AS total
FROM sales s JOIN regions g ON g.name = s.region
GROUP BY g.country
```

Semantica de NULL (estandar SQL):

- `COUNT(*)` cuenta filas; `COUNT(col)` ignora NULLs.
- `SUM`/`AVG`/`MIN`/`MAX` ignoran NULLs; sobre conjunto vacio devuelven NULL.
- Los NULL agrupan juntos en GROUP BY.
- `SUM` de int64 desborda con error explicito (no hace wrap); si mezcla int64 y float64 promociona a float64. `AVG` siempre devuelve float64.

Reglas:

- Toda columna no agregada del SELECT debe estar en GROUP BY.
- `SUM`/`AVG` exigen columnas int64 o float64.
- HAVING solo referencia columnas agrupadas y agregados (el agregado no necesita estar en el SELECT).
- En queries con agregados, ORDER BY referencia nombres de salida o aliases (`ORDER BY total DESC`), no llamadas a funcion: dale un alias al agregado.
- Los agregados solo viven en SELECT y HAVING (en WHERE usa... HAVING).
- No soportado: `COUNT(DISTINCT ...)`, agregados anidados, expresiones dentro del agregado.

## UPDATE

```sql
UPDATE users SET age = 31 WHERE id = 'u-admin'
UPDATE users SET email = NULL, age = 0 WHERE age > 100
```

- `SET` acepta solo literales (no `SET age = age + 1`; calcula en la aplicacion).
- Sin `WHERE` afecta todas las filas.
- Devuelve `QueryOutput::Affected(n)`.

## DELETE

```sql
DELETE FROM orders WHERE amount < 100
DELETE FROM orders          -- todas las filas
```

Devuelve `QueryOutput::Affected(n)`. Los snapshots vivos siguen viendo lo borrado hasta que se liberan.

## Como decide el planner

Heuristico, sin cost model:

1. `WHERE id = '...'` → point lookup directo.
2. Igualdad sobre columna indexada → busqueda por indice.
3. Cualquier otro filtro → full scan + filtro.
4. Los filtros de una sola tabla se empujan **antes** del join.
5. Joins: si el lado a sondear es chico (≤1024 filas) y la columna de join esta indexada (o es `id`), usa el indice por fila (index nested-loop); si no, hash join.

Regla practica: **indexa las columnas de join** (`orders.user_id`) y las columnas de busqueda frecuente.

## Fuera del subset V1

Todo esto falla con un error claro, no con comportamiento sorpresa:

| No soportado | Alternativa |
|---|---|
| `COUNT(DISTINCT ...)`, agregados anidados | deduplicar/calcular en la aplicacion |
| Subqueries, CTEs (`WITH`), `UNION` | reescribir como queries separadas |
| `FULL OUTER JOIN`, `CROSS JOIN` | dos queries + merge en la app |
| Aritmetica (`age + 1`) y funciones | calcular en la aplicacion |
| `LIKE`, `BETWEEN` | `BETWEEN` → `>= AND <=`; busqueda de texto llega con full-text (Phase 5) |
| `DISTINCT` | deduplicar en la app |
| `DROP`, `ALTER` | pendiente en el roadmap |
| `BEGIN/COMMIT` en SQL | transacciones via API Rust: `db.begin()` |
| `RETURNING` | INSERT ya devuelve los ids |
| Busqueda vectorial en SQL | API Rust: `db.create_vector_index(...)` + `db.search_vector(...)` (funcion SQL explicita llega con hybrid search, Phase 5) |

## Performance de referencia

Sobre 1M de ordenes + 10K usuarios (Apple Silicon, `cargo bench --bench sql`):

| Query | Latencia |
|---|---|
| Point lookup por indice unico (SQL completo: parse + plan + exec) | ~4 µs |
| JOIN indexado: usuario → sus ~100 ordenes de 1M, ORDER BY + LIMIT | ~360 µs |
| Full scan con filtro sin indice sobre 1M filas | ~1.1 s |

La ultima fila es el limite conocido de la spec ("joins grandes sin indice seran caros"): si una query es frecuente, indexala.
