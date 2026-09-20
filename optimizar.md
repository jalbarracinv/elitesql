# Optimizar EliteSQL

Actualización 2026-09-19: la continuación vigente está en
[las tareas TERRA](optimizar-tareas-terra.md) y los resultados más recientes en
[la iteración 02](benchmark-results/optimization-2026-09-19/iteration-02/README.md).
El índice compuesto y su recorrido ordenado están implementados; OFFSET evita
decodificar filas descartadas cuando el prefijo satisface todos los filtros.
La aceptación global T14/T15 continúa abierta. Los estados y cifras del
traspaso que sigue son históricos, no la certificación de estos cambios.

Nota de traspaso, 2026-09-14. Escrita para alguien que no ha visto este
repositorio antes. Si solo lees una cosa más, que sea
[el informe de la simulación](benchmark-results/saas-simulation-2026-09-12/README.md):
ahí están las 71 hipótesis probadas con su cifra, incluidas las que salieron
mal.

## Qué es esto

EliteSQL es una base de datos embebida escrita en Rust. Guarda filas con
control de versiones (MVCC), las escribe primero a un diario (WAL) y luego a
segmentos inmutables, y mantiene índices secundarios, de texto (BM25) y
vectoriales (HNSW). Se usa embebida, por un ABI en C, o como servidor por un
socket (`elitesql serve`). Tiene ligaduras para Python y Node.

El competidor de referencia es SQLite, porque ocupa el mismo hueco.

## La meta

Paridad con SQLite en lectura operacional **sin perder** la ventaja en
escritura concurrente e ingesta. Dos números concretos:

| objetivo | estado al cerrar |
|---|---|
| 90 % de SQLite en el barrido, conservando la latencia de cola | **cumplido** |
| operación ponderada por debajo de 60 µs | **no**: 83,3 |

Se puede romper la API y el formato en disco **si** las bases existentes se
convierten solas, sin pérdida y a prueba de `kill -9`.

## Dónde está cada cosa

| ruta | qué contiene |
|---|---|
| `crates/elitesql-core/src/db.rs` | el grueso del motor: estado, directorio primario, índices secundarios, lectura y escritura de filas. Es enorme; busca por nombre de función |
| `crates/elitesql-core/src/db/commit.rs` | el camino de commit: validación, WAL, aplicación, y el lote coordinado |
| `crates/elitesql-core/src/db/reads.rs` | lotes de lectura: barridos e igualdades |
| `crates/elitesql-core/src/db/maintenance.rs` | punto de control, compactación, publicación de tiradas |
| `crates/elitesql-core/src/paged.rs` | las tiradas inmutables paginadas sobre mmap, compartidas por todos los índices |
| `crates/elitesql-core/src/sql/exec.rs` | el ejecutor de sentencias |
| `crates/elitesql-core/src/sql/planner.rs` | elección del acceso a tabla (`table_driver`) |
| `crates/elitesql-core/src/value.rs` | codificación de valores, y la de claves de índice |
| `crates/elitesql-core/src/ddl.rs` | DDL con seguridad ante caídas, y las variantes de reescritura |
| `examples/saas_simulation/` | el banco de pruebas: una tienda con 16 operaciones, 76 % lecturas |

## Cómo está construido, lo justo para orientarse

- **Una fila** se encuentra por su **clave física**, una cadena. Desde esta
  sesión, si la tabla declara una columna `id`, esa clave **es** la identidad
  declarada; si no, es un ULID.
- **El directorio primario** mapea clave física a versión. Tiene dos partes:
  **tiradas** publicadas e inmutables, y **solapamientos** residentes con lo
  escrito desde el último punto de control. Regla de oro, y fuente de varias
  ganancias: *toda versión en un solapamiento es más nueva que cualquiera en
  una tirada*, así que hay que mirar los solapamientos primero y salir.
- **Un índice secundario** mapea valor de columna a clave física. Por eso una
  lectura por columna indexada son **dos saltos**: índice, luego directorio.
- **El commit** toma un mutex de serialización, valida contra el estado
  confirmado, escribe al diario y aplica bajo el candado de escritura del
  estado. Desde esta sesión, los commits que tocan filas disjuntas se aplican
  **en lote**, compartiendo una sola toma de ambos candados.
- **Los índices derivados son desechables**: si una tirada no valida su marca
  de formato, el cargador la reconstruye desde los datos canónicos. Esto hace
  los cambios de formato de índice mucho más baratos de lo que parecen.

## Cómo medir

```bash
cargo build --release                      # el motor y el servidor
cargo run --release -p elitesql-core --example statement_cost
python3 examples/saas_simulation/ops_cost.py
python3 examples/saas_simulation/sweep.py --transport sidecar --levels 10,100,500
python3 examples/saas_simulation/sweep.py --transport sqlite  --levels 10,100,500
```

- `statement_cost` mide **el motor a solas**, sin ligadura: coste por fila,
  por columna, por sentencia, y cuenta asignaciones con un asignador
  contador. Es la medida limpia.
- `ops_cost.py` mide **el coste por operación de la mezcla** contra SQLite.
  Siembra los dos motores en la misma corrida y hace punto de control antes
  de medir. Acepta `OPS_COST_PRODUCTS` y `OPS_COST_USERS`.
- `sweep.py` mide **concurrencia**, y además comprueba los invariantes de
  negocio y la integridad tras un `kill -9`. Acepta `--products`.

Para perfilar, en macOS: lanza `statement_cost --profile <segundos>
<point|indexed|update|scan|resident>` y engancha `sample <pid>`.

### Tres trampas que ya costaron tiempo

1. **Nunca dividas por una corrida de SQLite de otra sesión.** Hay que
   emparejarlas con minutos de diferencia. Una afirmación de esta sesión salió
   mal por esto y hubo que corregirla en el informe.
2. **El nivel de 500 usuarios varía un 17 %** entre corridas del mismo árbol.
   Cita un rango, no un número.
3. **Un bucle que escribe un millón de veces la misma fila mide el vaciado de
   memtable**, no el código de debajo. Un perfil señaló así una función que,
   al arreglarla, no movió nada.

Y una advertencia de estado: tras una sesión entera midiendo, la máquina
derivó un 25 % **con los dos motores a la vez**. Rehaz la línea base en frío
antes de la próxima tanda.

## Qué se hizo en esta sesión

Lista original de cinco puntos, por orden de ganancia esperada:

1. `Record` deja de ser `BTreeMap` — **ya estaba** hecho al empezar.
2. Clave física = identidad declarada — **hecho**. Incluye conversión
   automática de bases anteriores y pruebas de caída en las dos ventanas que
   deja un `kill -9`.
3. Alcanzar la versión visible sin buscar por fila — **cerrado por medición**.
   La mitad viable (solapamientos antes que tiradas) está hecha; la otra se
   paga con coste de escritura, con el argumento en el informe.
4. Commit con aplicación agrupada — **hecho**, y es lo que lleva el barrido de
   0,40 a 0,96 a quinientos usuarios.
5. Payload sin nombres de columna — **ya estaba**.

Además: búsqueda de texto de 276 a 125 µs, alcance de fila secuencial de 85 a
65 ns, con clave de 408 a 253 ns, y un defecto de corrección corregido (un
acierto de texto o vectorial escribía la clave física sobre el `id` declarado).

## Qué falta, con precios

De los 83,3 µs de la operación ponderada:

| parte | µs | se quita con |
|---|---:|---|
| convertir JSON a objetos de Python, solo en `browse` | ~10,6 | otra ligadura, no el motor |
| coste fijo de una llamada × 2,3 llamadas por operación | ~9 | casi agotado |
| trabajo proporcional a filas | el resto | **leer menos filas** |

Ojo con lo primero: el mismo `browse` cuesta 107 µs en el motor, 107 a través
del ABI en C con su JSON, y **151 a través de Python**. El motor ya responde
una sentencia pequeña en 1,46 µs, menos que la llamada entera de SQLite
(1,75). Decide primero si el objetivo va del motor o de lo que ve una
aplicación por esta ligadura: son dos números distintos.

**Lo único que llega a 60 µs es leer menos filas.** `browse` es el 36 % de la
ponderada y lee las 339 filas de una categoría para devolver veinte, porque el
único índice está sobre `category`. Un índice que satisficiera su `ORDER BY`
lo dejaría leer veinte: de ~151 a ~40 µs, y la ponderada a **unos 61**.

### Las tres piezas para eso

1. **Codificación de claves de índice que preserve el orden** — **hecha**
   (hipótesis 71, en `value.rs::encode_index_value`). Era el bloqueo real y no
   el planificador: `index_key` usaba la codificación general, que es
   little-endian, así que el 256 ordenaba antes que el 2 y ninguna tirada
   estaba ordenada por valor. Siete pruebas de propiedad lo fijan en
   `value.rs`, y `SECONDARY_FORMAT_VALUE` pasó a `ESQLSID3` para que las
   tiradas viejas se reconstruyan solas.

2. **Índices compuestos**, `(category, price_cents)`. `IndexDef` guarda una
   columna y solo diecinueve líneas la leen: doce en `db.rs`, seis en
   `ddl.rs`, una en el planificador. Debajo hay una capa que no se ve al
   principio: **la clave del par es `TAG || longitud(clave) || clave || id`**,
   y ese prefijo de longitud ordena por tamaño antes que por contenido e
   impide buscar por la primera columna de un compuesto. Hay que quitarlo.
   Para poder quitarlo, cada componente debe ser autodelimitado: los enteros
   y flotantes ya son de ancho fijo con su etiqueta, y el texto y los blobs ya
   llevan terminador `00 00` con el `00` escapado. Faltan JSON y vectores, que
   hoy caen a la codificación general. Con eso, un parseador de componentes
   permite que `secondary_pair_parts` separe clave e id sin longitud.
   Empecé esta pieza y **la revertí** para dejar el árbol verde; no queda nada
   a medias.

3. **Acceso ordenado en el planificador**. `table_driver` solo elige accesos
   por igualdad. Falta uno que recorra un índice en orden de clave y pare al
   juntar `offset + limit` filas, y que el ejecutor se salte la ordenación
   porque el acceso ya vino ordenado. El ordenador acotado ya sabe descartar
   sin materializar (`SpillSorter::may_keep`), así que esa mitad está puesta.

Las piezas 2 y 3 **solo se pueden medir juntas**: compuestos sin acceso
ordenado no ayudan, y acceso ordenado sin compuestos tampoco, porque ninguna
operación de la mezcla ordena por una columna indexada. Hazlas enteras o no
las empieces.

Y una decisión que no es técnica: añadir ese índice al esquema del banco
cambia qué se mide y afecta a los dos motores. Es de quien manda en el banco.

## No repitas esto

Siete hipótesis salieron negativas o neutras, cada una con su número y su
cifra en el informe: el cursor de avance sobre tiradas (48), cualquier tamaño
de página distinto de 1 KiB (49), el encabezado de clave en línea (53),
reindexar columnas que no cambiaron (57), mandar todos los commits por el
coordinador (63), admitir la tabla caliente al lote (66) y apartar solo al
miembro que choca (67).

## Reglas de la casa

Nada cuenta hasta que estén en verde, todas:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # 52 suites
cd bindings/python && ELITESQL_LIB=../../target/release python3 -m pytest -q
cd bindings/node && npm test
python3 examples/saas_simulation/sweep.py --transport sidecar --levels 10,100,500
```

El barrido cuenta como puerta por sus invariantes de negocio y su verificación
de integridad tras el `kill -9`, no por su rendimiento.

**No commitear.** El árbol se revisa y se commitea a mano.

## Método

Hipótesis, microtest, cambio, medición antes y después, **documentando también
lo negativo**. Cada hipótesis va al informe con su número, lo que se probó, lo
que se cambió y la cifra, ganara o perdiera. Siete de las once de esta sesión
perdieron, y están escritas con el mismo detalle que las que ganaron: eso es
lo que evita que la siguiente sesión las repita.
