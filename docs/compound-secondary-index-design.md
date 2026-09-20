# Diseño: índices secundarios compuestos y recorrido ordenado

Estado: implementado y validado localmente, 2026-09-19.

El primer caso es `(category, price_cents)`: una consulta con igualdad en
`category`, `ORDER BY price_cents ASC` y `LIMIT/OFFSET` debe evitar leer y
ordenar la categoría completa.

## Catálogo y compatibilidad

Un índice contiene una lista ordenada no vacía de columnas y `unique`. La API
actual de una columna es un adaptador de una lista de longitud uno. El catálogo
continúa leyendo la propiedad antigua `column: "x"`; las definiciones nuevas
escriben `columns: ["x", "y"]` y mantienen `column` igual al primero mientras
haya lectores de catálogo anteriores.

La identidad interna es tabla más lista completa de columnas. Rutas y
manifiestos usan un identificador sin colisiones, formado por longitudes
big-endian y bytes UTF-8 de los nombres. `(a)`, `(a,b)` y `(a,c)` coexisten.
Renombrar actualiza toda lista y eliminar una columna elimina los índices que
la contienen. Las tiradas derivadas son desechables y se reconstruyen desde
datos canónicos si su marcador de formato no corresponde.

## Claves y pares secundarios

La clave lógica concatena los componentes de `encode_index_value` en orden.
Enteros, fechas/horas, floats normalizados, texto y blobs preservan orden
binario; JSON y vectores son autocontenidos para igualdad, pero no habilitan
recorrido ordenado. Una entrada persistida es:

```text
SECONDARY_ENTRY_TAG || clave_de_tupla || 0xff || id_fisico_utf8
```

El separador se busca desde el final. Un id UTF-8 válido no contiene `0xff`,
por lo que separa la identidad aunque una clave tenga bytes arbitrarios. El
prefijo de longitud anterior desaparece. Un prefijo de componentes completos
es entonces válido para un cursor. El nuevo marcador es `ESQLSID4`; `ESQLSID3`
y anteriores fuerzan rebuild. Un tag, separador o UTF-8 inválido da
`Error::Corrupt`, nunca panic.

## NULL, unicidad y orden

Las entradas compuestas contienen NULL, para que un recorrido ordenado no
omita filas. NULL ordena primero; `col = NULL` sigue sin producir filas. Un
índice UNIQUE no da conflicto si algún componente es NULL.

La primera versión elimina `SpillSorter` solo si cada columna de prefijo tiene
igualdad no NULL, el ORDER BY restante coincide ascendentemente con el resto
del índice y la collation es binaria compatible. Unicode, DESC, direcciones
mixtas, JSON, vectores, snapshots históricos y transacciones staged usan el
camino existente. Los empates se desempatan por id físico en pruebas exactas.

## Visibilidad y cursor

Los secundarios representan estado actual. Si el snapshot no es el commit
actual, o hay cambios staged, el driver ordenado no se usa. Para lectura actual
cada lote toma el lock de estado, fusiona tiradas, delta, removidos y delta
congelado por `(clave_de_tupla, id)`, retiene los lectores de segmento
necesarios y lo libera antes de decodificar. La operación más nueva gana y los
tombstones se omiten. La continuación es ese par completo, incluso tras un
lote filtrado vacío. Si el snapshot deja de ser actual antes de adquirir el
lote, el ejecutor reinicia por scan y sort sobre ese mismo snapshot antes de
producir una fila.

Cuando todos los predicados son igualdades cubiertas por el prefijo, OFFSET
cuenta entradas vivas del índice sin recuperar ni decodificar sus registros.
Los filtros adicionales conservan la evaluación fila por fila. El cursor
residente reanuda desde la tupla y el id anteriores mediante rangos B-tree.
Si un commit invalida el recorrido, el reinicio usa scan sobre el snapshot
original, incluso si existe un índice de igualdad del estado actual.

El lock no se conserva durante la consulta ni se copia el índice entero. Cada
lote se limita por filas y por la mitad del presupuesto de trabajo; cancelación
y proyección se aplican antes de materializar.
Si no hay vista consistente, se toma fallback antes de devolver resultados.

## Casos de aceptación

- distinta longitud, cero embebido, extremos numéricos, ambos ceros float,
  texto UTF-8, JSON y vector;
- NULL en cada posición, UNIQUE, updates de cada componente, borrados y runs;
- checkpoint, compactación, reapertura, corrupción y `kill -9` durante rebuild;
- equivalencia con scan+sort para offset, empates y filtros residuales;
- snapshots antiguos, commits concurrentes, staged writes, memoria y cancelación.
