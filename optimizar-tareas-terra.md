# Plan de tareas de optimización para GPT TERRA

Fecha: 2026-09-19. Estado: **capacidad principal implementada; aceptación de rendimiento incompleta**.

Este documento convierte la revisión de [optimizar.md](optimizar.md) en una
secuencia ejecutable. No reemplaza el plan histórico `plan.md` ni convierte las
estimaciones del informe anterior en resultados nuevos.

## Objetivo y alcance

Reducir el trabajo de las lecturas operacionales, empezando por:

```sql
CREATE INDEX ON products (category, price_cents);

SELECT id, name, price_cents, stock
FROM products
WHERE category = ?
ORDER BY price_cents ASC
LIMIT ? OFFSET ?;
```

El resultado esperado es que una página evite recorrer y ordenar todos los
productos de la categoría. Debe conservar corrección SQL, snapshots, recuperación,
memoria acotada y las ventajas de escritura concurrente e ingesta.

La entrega principal incluye índices compuestos **y** ejecución ordenada. No
declarar la optimización terminada después de implementar solo una de esas piezas.

Primera versión del acceso ordenado:

- SELECT de una tabla, sin agregación ni joins, con LIMIT.
- Prefijo de columnas fijado por igualdades y ORDER BY compatible con el resto.
- Recorrido ascendente. DESC y direcciones mixtas mantienen el sort existente;
  su aceleración se evalúa después de medir la primera entrega.
- Orden de enteros, booleanos, fechas/horas y texto binario compatible con SQL.
  Float, JSON, vectores y texto con collation Unicode conservan el sort hasta
  demostrar equivalencia de orden. Esto no elimina sus capacidades existentes.
- Las transacciones con escrituras pendientes y los snapshots históricos
  conservan inicialmente el camino correcto existente. No usar un índice del
  estado actual como si contuviera la historia.
- Mantener las API de índice de una columna. Los compuestos se pueden exponer
  inicialmente mediante SQL, que ya atraviesa Rust, FFI, Python y sidecar.

No incluye reescribir todo `db.rs`, cambiar el coordinador de commits, introducir
índices de cobertura, sustituir el binding Python ni rediseñar WAL o MVCC.

## Instrucciones de ejecución

1. Trabajar secuencialmente, una tarea activa por vez. No delegar ni crear agentes
   salvo instrucción explícita del usuario.
2. Antes de editar, leer los archivos indicados y comprobar `git status`. Preservar
   cambios ajenos. **No commitear, publicar ni ejecutar reset del árbol.**
3. Cerrar cada tarea con sus pruebas relevantes y evidencia. No ejecutar toda la
   matriz de benchmarks después de cada cambio pequeño; reservarla para hitos.
4. No hacer refactors incidentales, añadir dependencias o cambiar protocolos sin
   necesidad demostrada. Si una tarea crece demasiado, dividirla aquí en subtareas
   verificables antes de seguir, manteniendo sus criterios de aceptación.
5. No pedir confirmación para cada elección rutinaria dentro del alcance cuando
   el usuario autorice ejecutar este plan. Resolverla con código y medición.
   Respetar los permisos reales del entorno y explicar bloqueos concretos.
6. No tocar bases del usuario. Usar fixtures, copias y directorios temporales
   exclusivos de cada corrida; no limpiar rutas compartidas de nombre fijo.
7. Las pruebas de caída deben matar únicamente procesos creados por el harness.
8. En cada reanudación, leer el registro de avance y continuar la primera tarea
   pendiente cuyas dependencias estén completas. No repetir mediciones válidas
   salvo cambios o dudas que las invaliden.

Lecturas iniciales obligatorias:

- [optimizar.md](optimizar.md).
- El traspaso y las hipótesis pertinentes del
  [informe de simulación](benchmark-results/saas-simulation-2026-09-12/README.md).
- [scripts/acceptance.sh](scripts/acceptance.sh).
- [docs/disk-format.md](docs/disk-format.md) y
  [docs/recovery.md](docs/recovery.md) antes de modificar persistencia.

No repetir sin evidencia nueva las hipótesis negativas 48, 49, 53, 57, 63, 66 y 67.
Asignar números nuevos a los experimentos comprobando primero el último número
del informe; durante esta revisión llegaba a 71.

## Hallazgos que el implementador debe preservar

| Hecho observado | Consecuencia para las tareas |
|---|---|
| `IndexDef` contiene `column`, y CREATE/DROP INDEX rechazan listas | Cambiar catálogo, ciclo de vida y sintaxis, no solo el planificador |
| El par secundario antepone longitud a la clave | El orden del valor codificado todavía no es el orden físico del par |
| `SecIdx.delta`, `removed` y sus versiones congeladas usan `HashMap` | Hace falta una estrategia ordenada también para los cambios residentes |
| Los índices secundarios actuales omiten NULL y reflejan el estado reciente | Resolver cobertura de NULL y visibilidad antes de quitar el sort |
| La continuación de lectura actual usa identidad de fila | Un recorrido compuesto necesita continuar por clave completa e identidad |
| El orden textual del índice es binario y ORDER BY usa Unicode por defecto | No declarar compatible una ordenación de texto por el mero nombre de columna |
| El executor ya tiene proyección selectiva y top-k acotado | Reutilizarlos; la ganancia buscada es visitar menos entradas y filas |
| `ops_cost.py` tiene 11 operaciones, pesos que suman 89 y divisor 100 | La cifra histórica es una suma parcial, no la media completa de la simulación |
| La simulación tiene 16 pesos que suman 98,3 y `random.choices` los normaliza | Compartir definición de mezcla; no confundir normalización con error del simulador |
| El microbenchmark usa páginas 0–2; el sweep usa páginas 0–4 | Para LIMIT 20 se necesitan 20/40/60 y hasta 100 coincidencias con OFFSET |
| Hay selecciones de producto limitadas a 5.000 y el carrito añade/elimina IDs distintos | Corregir escala y deriva del estado del microbenchmark |

Las cifras de 83,3 µs y 151 µs son históricas. La proyección
`83,3 - 0,20 × (151 - 40) = 61,1 µs` no demuestra llegar a menos de 60.
Con los mismos datos, `(151 - 107,5) × 0,20 = 8,7 µs`; no reutilizar la
estimación de 10,6 µs atribuida a esa conversión sin explicar otra base de cálculo.

## Secuencia y dependencias

```text
T00 → T01 → T02 → T03 → T04 → T05 → T06 → T07 → T08
    → T09 → T10 → T11 → T12 → T13 → T14 → T15
```

Las pruebas acompañan cada implementación. T13 integra y amplía la cobertura;
no es una autorización para posponer corrección hasta el final.

### T00 — Preparar el entorno y registrar el punto de partida

**Depende de:** ninguna. **Tipo:** preparación, sin cambios funcionales.

- Comprobar HEAD, diff, herramientas, dependencias y sistema operativo.
- Confirmar disponibilidad de Rust, Python, Node y bibliotecas nativas. En el
  entorno de la revisión no había `cargo` en PATH ni en las rutas habituales,
  y no existía `target/`; volver a comprobarlo, no asumir que sigue igual.
- Crear un directorio de resultados propio para esta ejecución, con versiones,
  configuración, comandos y salidas. Si hay cambios sin commit, guardar un diff
  que identifique el código medido; HEAD por sí solo no basta.
- Ejecutar la aceptación inicial disponible. Clasificar fallos previos y fallos
  de entorno sin atribuirlos a esta optimización.
- Conservar un ejecutable/fixture de referencia del formato anterior para T05 y
  T13. No fabricar una prueba de migración cambiando solo el marcador del formato.

**Aceptación:** entorno reproducible, estado inicial documentado y referencia
anterior identificada. Sin herramientas no declarar pruebas ni baseline verdes;
continuar solo trabajo independiente del bloqueo.

### T01 — Corregir el benchmark sin borrar la comparación histórica

**Depende de:** T00. **Archivos:** `examples/saas_simulation/ops_cost.py`,
`saas/vuser.py`, `saas/schema.py`, `saas/service.py` y documentación del harness.

- Conservar un modo histórico claramente identificado. Añadir una versión
  corregida con identificador de métrica, pesos, operaciones y escala en la salida.
- Obtener la definición de pesos de una fuente común. Para el subconjunto medido,
  mostrar cobertura y media normalizada; no llamarlo mezcla completa.
- Añadir medición de las 16 operaciones con precondiciones reproducibles. Sembrar
  y restaurar estado fuera del intervalo cronometrado; registrar resultado de
  negocio, intentos y errores. No comparar operaciones exitosas con abortos vacíos.
- Usar el número configurado de productos en todas las selecciones y validar las
  escalas mínimas soportadas. Hacer que el carrito conserve el tamaño previsto;
  su limpieza no debe inflar silenciosamente el coste de `add_to_cart`.
- Repetir y alternar el orden de los motores; guardar muestras y dispersión,
  configuración de SQLite, durabilidad, estado de checkpoint y versión de código.
- Documentar que `recommend` hace ANN en EliteSQL y un fallback distinto en SQLite;
  separar esa comparación de consultas SQL equivalentes.
- Usar temporales exclusivos por ejecución.

**Aceptación:** prueba corta a dos escalas; pesos/cobertura verificables, ninguna
selección fuera de escala, carrito estable y misma secuencia para ambos motores.
Conservar datos de las métricas histórica y corregida por separado.

### T02 — Obtener la línea base antes de tocar el motor

**Depende de:** T01. **Archivos:** harness, `statement_cost.rs`, resultados.

- Compilar release y medir motor solo, Python embebido y sidecar por separado.
- Usar 5.000 y 50.000 productos; distinguir filas residentes y publicadas.
- Medir páginas iniciales y posteriores, throughput, p95/p99, errores/reintentos,
  memoria, igualdad puntual, commit concurrente e ingesta.
- Emparejar al menos tres repeticiones EliteSQL/SQLite en la misma sesión, sin
  otros benchmarks simultáneos. Registrar rangos, especialmente a 500 usuarios.
- Mantener el esquema original para esta línea base.
- Declarar explícitamente que la meta histórica de 60 µs pertenece a la métrica
  histórica. La mezcla completa corregida tendrá una base nueva; no transferir
  el umbral ni declarar éxito cambiando el denominador.

**Aceptación:** tabla antes, comandos reproducibles y resultados brutos. Elegir
desde aquí los escenarios de escritura/ingesta que se repetirán en T14.

### T03 — Fijar contratos de formato, orden y visibilidad

**Depende de:** T02. **Tipo:** diseño breve y casos de aceptación antes del código.
**Archivos a leer:** `schema.rs`, `value.rs`, `db.rs`, `run_manifest.rs`,
`db/commit.rs`, `ddl.rs`, `sql/planner.rs`, `sql/values.rs`, `collate.rs`.

- Especificar definición de índice por lista ordenada de columnas, identidad
  interna sin colisiones ambiguas y compatibilidad con el catálogo antiguo.
- Definir formato autodelimitado de tupla/par, ubicación de identidad, marcador
  de versión y comparación. No separar columnas con una coma sin escape.
- Separar tres reglas: qué filas están en el índice, cuáles participan en
  unicidad y cuándo un ORDER BY es compatible. Incluir tuplas con NULL para
  cobertura; una tupla con algún NULL no genera conflicto UNIQUE.
- Definir continuaciones, límites inclusivos/exclusivos y desempate físico.
  Las pruebas de equivalencia exacta entre motores deben agregar un ORDER BY
  determinista cuando haya empates; no cambiar el workload principal en secreto.
- Definir cómo adquirir la vista de índice y el snapshot de forma consistente,
  liberar el candado, conservar segmentos y continuar sin saltos ni duplicados.
- Para snapshots antiguos y escrituras pendientes usar el camino existente de
  lectura y ordenación. Si no se puede adquirir una vista estable del camino
  nuevo dentro del presupuesto, elegir fallback antes de emitir resultados.

**Aceptación:** contrato escrito con ejemplos de bytes, casos de NULL, Unicode,
mutaciones concurrentes y migración. No comenzar T04 con la visibilidad indefinida.

### T04 — Extraer los internos secundarios sin cambiar comportamiento

**Depende de:** T03. **Archivos:** `db.rs` y nuevo `db/secondary.rs` o equivalente.

- Extraer `SecIdx`, sus deltas, cursores y codificación de pares relacionados.
- Mantener responsabilidades de candados y publicación identificadas.
- No cambiar todavía formato, contenedores, API ni planificador.
- Evitar visibilidad pública nueva solo para resolver la extracción.

**Aceptación:** diff principalmente mecánico, `cargo check --workspace --all-targets
--locked`, formato y suites `secondary_runs`, `derived_run_recovery`,
`txn_indexed_reads` e `integrity_regressions` verdes.

### T05 — Implementar el nuevo formato de clave secundaria

**Depende de:** T04. **Archivos:** `value.rs`, internos secundarios, reconstrucción.

- Eliminar el prefijo de longitud que antecede al contenido ordenable.
- Codificar y delimitar componentes y final de tupla sin ambigüedad. Mantener
  codificación de payload independiente de codificación de índice.
- Cubrir todos los tipos admitidos actualmente por índices escalares. JSON
  necesita framing aunque siga sin ofrecer acceso ordenado; no habilitar por
  accidente índices escalares vectoriales que la API rechaza hoy.
- Validar truncamientos, terminadores inválidos y tamaños sin panic ni lecturas
  fuera de límites. Usar las mismas funciones en escritura, lectura y rebuild.
- Cambiar el marcador del formato y reconstruir tiradas anteriores desde datos
  canónicos mediante los mecanismos de publicación existentes.

**Aceptación:** propiedades de orden y separación, claves de distinta longitud,
ceros embebidos, negativos, extremos y ceros flotantes. Apertura de fixture real
anterior, reconstrucción, segunda apertura y comprobación de integridad correctas.
No anunciar mejora de consultas todavía.

### T06 — Generalizar catálogo e identidad del índice

**Depende de:** T05. **Archivos:** `schema.rs`, `run_manifest.rs`, internos
secundarios, `ddl.rs`, reconstrucción, check/repair/backup donde corresponda.

- Sustituir la suposición de una columna por una lista ordenada no vacía.
- Leer definiciones antiguas de una columna y publicar el catálogo actualizado
  con el mecanismo durable existente. No reescribir a mano el manifiesto activo.
- Propagar identidad del índice a mapas, rutas, manifiestos y limpieza de archivos.
- Centralizar construcción de claves desde un registro; usarla al reconstruir,
  aplicar cambios, comprobar integridad y validar unicidad.
- Revisar consumidores por responsabilidad; no confiar en el conteo histórico
  de diecinueve referencias, que no representa todo el alcance funcional.
- Mantener activa solo la funcionalidad de una columna hasta completar T07.

**Aceptación:** operaciones actuales y migración siguen funcionando; ningún
archivo derivado válido se borra por una confusión de nombres. Apertura en modo
solo lectura respeta el contrato existente y no escribe una migración oculta.

### T07 — Completar índices compuestos de extremo a extremo

**Depende de:** T06. **Archivos:** parser/AST/ejecutor DDL, API Rust, `ddl.rs`,
`db/commit.rs`, mantenimiento, validación de esquema e índices.

- Aceptar CREATE [UNIQUE] INDEX y DROP INDEX con listas ordenadas de columnas.
- Validar columnas inexistentes, repetidas y tipos no admitidos. Permitir que
  coexistan `(a)`, `(a,b)` y `(a,c)` según la identidad fijada en T03.
- Crear/reconstruir índices sobre datos existentes y mantenerlos en INSERT,
  UPDATE, DELETE, replay, checkpoint y compactación.
- Actualizar rename/drop de columnas y tabla. Aplicar a compuestos la política
  existente de eliminación de índices afectados, sin dejar referencias huérfanas.
- Validar UNIQUE sobre la tupla completa, incluyendo conflictos dentro del
  mismo commit y commits concurrentes. No tratar su prefijo como único para
  búsquedas puntuales ni para validar claves foráneas de una columna.
- Mantener la API `create_index(table, column, unique)` como adaptador. Probar
  creación SQL desde Python y sidecar sin exigir una nueva ABI.
- Mantener NULL en las entradas para cobertura, pero conservar `col = NULL`
  como no coincidente y la exclusión de NULL de conflictos UNIQUE.

**Aceptación:** suites nuevas de compuestos más `sql_foreign_keys`, `ddl`,
`secondary_runs`, `txn_indexed_reads`, `sql_null_logic` y recuperación. Las
consultas aún pueden ordenar por el camino anterior; T07 no cierra la entrega.

### T08 — Añadir acceso ordenado a las estructuras residentes

**Depende de:** T07. **Archivos:** internos secundarios y contabilidad de memoria.

- Prototipar primero una representación ordenada sencilla de delta/removidos
  y versión congelada, conservando búsquedas de igualdad eficientes.
- Comparar con la representación anterior: igualdad, inserción, cambio de clave,
  borrado, commit e ingesta. Medir varias cardinalidades y tamaño de delta.
- Si usar un árbol ordenado introduce regresión reproducible, evaluar una
  alternativa acotada; no mantener dos copias completas sin medir y contabilizar
  su coste. Documentar experimentos descartados.
- Contabilizar claves compuestas y estructuras auxiliares en admisión y memoria.

**Aceptación:** recorrido por prefijo/rango ordenado en memoria, tombstones
correctos, límites de memoria respetados y decisión de representación sustentada.

### T09 — Implementar el cursor ordenado sobre todas las capas

**Depende de:** T08. **Archivos:** `paged.rs`, internos secundarios, `db/reads.rs`.

- Recorrer tiradas por prefijo y límites y combinarlas con deltas activos y
  congelados por `(clave de índice, identidad)`.
- Resolver adiciones/borrados según precedencia y versión; no resucitar entradas
  antiguas ni devolver la misma fila dos veces tras un cambio de clave.
- Adquirir una vista estable ligada al snapshot según T03. Retener las tiradas,
  segmentos y datos necesarios; un `Arc` a estado mutable no es una vista estable.
- Mantener memoria acotada y candados breves. No sostener el candado de estado
  durante toda una consulta ni copiar indiscriminadamente el índice entero.
- Continuar con una clave completa, incluso si el lote produjo cero filas tras
  filtrar. Integrar cancelación y límites de bytes además de número de filas.
- Ante falta de vista o memoria, usar fallback antes de entregar filas; nunca
  mezclar medio resultado ordenado con otro recorrido sin orden compatible.

**Aceptación:** datos residentes, publicados y mixtos producen la misma secuencia
que el oráculo; pruebas con múltiples tiradas, borrados, cambio de claves y
checkpoint/compactación entre lotes. Coste de adquisición de vista medido.

### T10 — Refactorizar el plan físico y actualizar EXPLAIN

**Depende de:** T09. **Archivos:** `sql/planner.rs`, consumidores de `TableDriver`.

- Representar acceso, prefijo/rango, dirección, filtros residuales y garantía de
  orden en una decisión compartida entre ejecución y EXPLAIN.
- Preferir point lookup cuando devuelve como máximo una fila. Reconocer el caso
  `(category, price_cents)` con igualdad en `category` y orden por `price_cents`.
- Comprobar todas las claves ORDER BY, tipos, collation y visibilidad; una
  coincidencia parcial no permite eliminar toda la ordenación.
- Preservar el comportamiento de joins, UPDATE/DELETE, cursores públicos y
  transacciones que no se beneficien de esta primera entrega.
- Mostrar en EXPLAIN cuándo el orden viene del índice y cuándo queda un SORT,
  sin inventar cardinalidades o tiempos estimados.

**Aceptación:** pruebas `sql_explain` y nuevas de selección del plan cubren
compatible, incompatible, Unicode, DESC, NULL, prefijo parcial y unicidad compuesta.

### T11 — Ejecutar ORDER BY con corte temprano

**Depende de:** T10. **Archivos:** `sql/exec.rs`, `db/reads.rs`, `sql/sort.rs` si hace falta.

- Construir `SpillSorter` solo cuando el plan no garantiza todo el orden pedido.
- Aplicar predicados residuales antes de contar coincidencias para OFFSET/LIMIT.
- Pedir lotes proporcionados al resultado restante. Evitar decodificar columnas
  y filas descartables cuando la proyección existente lo permita.
- Manejar LIMIT 0, offset mayor al total y overflow de offset + limit.
- Preservar el sort y las lecturas existentes como camino alternativo verificable.

**Aceptación:** consulta objetivo correcta con primeras páginas y offsets altos;
sin filtros residuales, sin churn y con una sola tirada, el trabajo se aproxima
a OFFSET + LIMIT, con el overhead acotado y documentado del cursor. Con filtros,
borrados o múltiples versiones puede haber más visitas: no falsear ese contador.

### T12 — Instrumentar y añadir el escenario de benchmark con compuestos

**Depende de:** T11. **Archivos:** medición del motor, harness y documentación.

- Medir entradas visitadas, filas recuperadas, filas decodificadas, filas devueltas
  y uso del sorter. Distinguir contadores lógicos de lecturas físicas de páginas.
- Mantener instrumentación local/opt-in cuando sea posible; no agregar atomics
  globales por fila al camino caliente sin medir su coste.
- Añadir un escenario explícito `compound-index` junto al escenario original.
  Crear `(category, price_cents)` en ambos motores. Mantener los otros índices
  constantes para aislar el efecto; quitar redundantes sería otro experimento.
- Guardar planes de ambos motores. Un benchmark favorable sin uso del plan
  esperado no valida la hipótesis.
- Mantener iguales consulta, proyección, distribución y páginas por escenario.

**Aceptación:** se puede reproducir original y compuesto sin editar manualmente
el esquema; los resultados identifican escenario y versión de métrica.

### T13 — Integrar pruebas adversariales y migración interrumpida

**Depende de:** T12. **Archivos:** suites existentes y nuevas de índices/SQL/crash.

- Comparar recorrido ordenado con scan + sort bajo operaciones aleatorias:
  inserts, updates de cada componente, deletes y múltiples checkpoints.
- Probar NULL en cada posición, claves duplicadas, empates, texto vacío/ceros,
  límites numéricos, parámetros y filtros residuales muy selectivos.
- Probar snapshots abiertos antes de cambios, escrituras pendientes y commits
  mientras una lectura cambia de lote. No omitir pruebas del fallback.
- Inyectar fallos y `kill -9` en las ventanas reales de publicación del catálogo
  y reconstrucción de tiradas. Reabrir repetidamente, verificar filas y unicidad.
- Probar tiradas/manifiestos ausentes o dañados, catálogo anterior, backup/restore,
  memoria pequeña, cancelación y limpieza de temporales.
- En pruebas diferenciales con SQLite usar semántica compartida; para Unicode u
  otras diferencias documentadas, el oráculo es el ejecutor anterior de EliteSQL.

**Aceptación:** ninguna pérdida/duplicación de filas, resurrección de borrados,
violación de unicidad, resultado parcial exitoso tras error ni crecimiento de
memoria sin presupuesto. Evidencia de recuperación tras una segunda reapertura.

### T14 — Medir la entrega y pasar las puertas completas

**Depende de:** T13.

- Repetir las mediciones de T02, emparejadas y con al menos tres repeticiones por
  escenario principal. No reutilizar una referencia SQLite de otra sesión.
- Comparar esquema original antes/después para detectar regresiones del motor,
  y escenario compuesto en ambos motores para medir la capacidad nueva.
- Separar coste de la nueva estructura de deltas del coste de mantener un índice
  adicional; mostrar ambos en escrituras/ingesta.
- Informar resultados por operación, páginas, escala, memoria y estado residente/
  publicado. Reportar también resultados negativos.
- Ejecutar las puertas de `optimizar.md` y `scripts/acceptance.sh`; incluir pruebas
  Node de integración, no solo `npm test`, y el sort bajo límite de descriptores.

Comandos de referencia desde la raíz (ajustar solo por diferencias documentadas
del entorno):

```bash
bash scripts/acceptance.sh
cargo build --release --workspace --locked
ELITESQL_LIB="$PWD/target/release" python3 -m pytest -q bindings/python/tests
npm --prefix bindings/node test
npm --prefix bindings/node run test:integration
cargo run --release -p elitesql-core --example statement_cost
python3 examples/saas_simulation/ops_cost.py
python3 examples/saas_simulation/sweep.py --transport sidecar --levels 10,100,500
python3 examples/saas_simulation/sweep.py --transport sqlite --levels 10,100,500
```

Agregar las opciones nuevas de escenario/métrica implementadas en T01/T12 al
registro de comandos; no asumir que los comandos predeterminados cubren ambos.
Los sweeps deben incluir invariantes de negocio e integridad tras `kill -9`.

**Aceptación funcional:** todas las puertas relevantes verdes. No declarar una
suite verde solo porque hay un resultado histórico o porque la herramienta falta.

**Aceptación de rendimiento:** reducción reproducible del trabajo de browse y
su latencia; objetivo de throughput de al menos 0,90× SQLite por nivel, con
rangos y p95/p99 de ambos motores. Investigar degradaciones repetibles que
excedan la variabilidad de la línea base en igualdad, commit, ingesta y memoria.
No relajar durabilidad, aumentar presupuesto o cambiar mezcla para ocultarlas.

Informar por separado si se alcanzan menos de 60 µs en la métrica histórica y
cuál es la nueva media corregida. Si la meta numérica falla, la capacidad puede
estar implementada, pero el objetivo de rendimiento permanece pendiente.

### T15 — Actualizar documentación y preparar el siguiente traspaso

**Depende de:** T14. **Archivos:** `optimizar.md`, informe de hipótesis, manual,
documentos de formato/recuperación y clientes cuando corresponda.

- Documentar SQL compuesto, compatibilidad, NULL/UNIQUE, acceso ascendente,
  restricciones de collation/snapshot y cuándo sigue apareciendo SORT.
- Actualizar las afirmaciones afectadas; no dejar que README, manual y catálogo
  describan capacidades o formato anteriores.
- Publicar tablas antes/después con comandos, código medido, escenarios,
  incertidumbre y enlaces a muestras. No sobrescribir evidencia histórica.
- Dejar pendientes concretos y decisiones justificadas. Registrar las hipótesis
  negativas con el mismo detalle que las positivas.
- Revisar diff final y estado del árbol. No crear commits.

**Aceptación:** otro agente puede reproducir la entrega y saber qué está completo,
qué sigue pendiente y si las metas se alcanzaron realmente.

## Tareas condicionales: solo después de T14

No ejecutarlas automáticamente como parte de la primera entrega. Elegir la
siguiente por el reparto de coste medido y registrar su hipótesis antes de cambiar.

| ID | Oportunidad | Condición para empezar | Evidencia requerida |
|---|---|---|---|
| C01 | Cursor DESC y ordenaciones compatibles adicionales | Ordenaciones descendentes aportan coste material | Equivalencia de NULL/empates y mejora de últimas reseñas/historial |
| C02 | `reviews(product_id, created_ms)` y `orders(user_id, created_ms)` | C01 disponible y suficientes filas por prefijo | Ganancia de lectura frente a coste de escritura y espacio |
| C03 | `cart_items(cart_id, product_id)` y `carts(user_id, status)` | Igualdades múltiples aparecen entre los costes principales | Plan de igualdad por tupla/prefijo, consultas más baratas y commits estables |
| C04 | Menor conversión de resultados en Python | Perfil posterior muestra coste dominante del binding | Comparación Rust/ABI/Python, preservación de tipos y coste de distribución |
| C05 | Rangos SQL mediante el mismo cursor | Predicados de rango justifican extender el planificador | Límites exactos, NULL, filtros residuales y medición por selectividad |
| C06 | Paginación por cursor | OFFSET profundo domina una carga real | Benchmark separado y contrato de API explícito; no reemplazar OFFSET del benchmark histórico |

Los índices de cobertura y un optimizador basado en estadísticas quedan fuera
de este ciclo. No empezar una reescritura del motor para alcanzar una cifra.

## Registro de avance

Avance 2026-09-19, iteración 02: se midió y optimizó el coste de OFFSET,
la continuación residente y la construcción de salida JSON. Se añadió una
prueba determinista del fallback a snapshot al cambiar el commit entre lotes,
además de equivalencia con scan/sort bajo mutaciones y presupuesto pequeño.
Resultados y limitaciones en
[el informe de iteración](benchmark-results/optimization-2026-09-19/iteration-02/README.md).
T14/T15 permanecen abiertos; esta iteración no acredita la matriz completa.

Actualizar esta tabla al cerrar cada tarea. Estados: pendiente, en curso,
completa o bloqueada. Una tarea bloqueada debe indicar causa verificable y qué
trabajo independiente puede continuar.

| Tarea | Estado | Evidencia / limitación |
|---|---|---|
| T00 | Completa | Rust 1.89 instalado; fmt y `cargo test --workspace --locked` verdes; fixture ESQLSID3 y detalle en `benchmark-results/optimization-2026-09-19/T00-T02-baseline.md`. |
| T01 | Completa | `full-v2` comparte 16 pesos con `vuser`, normaliza 98,3, alterna repeticiones y guarda JSON; smoke EliteSQL/SQLite a 100 y SQLite a 50.000 verdes. |
| T02 | Completa | Base 5.000/50.000, statement cost, write cost y sweeps 10/100/500 en `benchmark-results/optimization-2026-09-19/T00-T02-baseline.md`; `check` de EliteSQL tiene warnings derivados pendientes de repetir. |
| T03 | Completa | Contrato de catálogo, claves, NULL, fallback MVCC, cursor y migración en `docs/compound-secondary-index-design.md`. |
| T04 | Completa | Internos extraídos a `db/secondary.rs` sin cambio semántico inicial; formato y suites focalizadas verdes. |
| T05 | Completa | `ESQLSID4` usa clave ordenable antes de identidad; fixture ESQLSID3 real se reconstruye, reabre y pasa `check`; también se corrigió la comparación lógica para usar `encode_index_value`. |
| T06 | Completa | Catálogo compatible con `columns`, identidad de índice sin colisiones, migración durable y rutas/manifiestos compuestos; fixture anterior y `check` validados. |
| T07 | Completa | SQL/API para CREATE/DROP de listas, tupla UNIQUE con NULL, actualización en escritura/replay/DDL y suites de compuestos, DDL, FK, NULL, runs y transacciones verdes. |
| T08 | Completa | Deltas, removidos y delta congelado usan `BTreeMap`; el recorrido por prefijo se prueba con overlay residente y no duplica la estructura completa. |
| T09 | Completa | Cursor físico `(tupla,id)` fusiona runs, overlay y tombstones, limita bytes/filas, respeta cancelación y toma fallback si el snapshot pasa a ser histórico antes del primer lote. |
| T10 | Completa | Plan compartido `OrderedSecondaryPlan`, admisión y EXPLAIN muestran `INDEX ORDERED` únicamente para prefijo, orden, tipo y collation compatibles. |
| T11 | Completa | SELECT de una tabla con LIMIT usa el cursor ordenado, aplica filtros antes de OFFSET/LIMIT y conserva scan+sort como fallback. |
| T12 | Completa | `statement_cost` compara la página original/compuesta; `ops_cost.py --scenario compound-index` crea el índice en ambos motores y guarda escenario en JSON. |
| T13 | Completa | Cobertura de compuestos incluye runs+overlay, update/delete, NULL/UNIQUE, DDL, reapertura e integridad; la matriz workspace cubre recuperación, corrupción, snapshots y churn. |
| T14 | En curso | Iteración 02: paginación 5.000/50.000, seis bloques por versión de full-v2 y un sweep emparejado 10/100/500. Media agrupada 77,13 → 72,84 µs; throughput final 0,483/0,584/0,608× SQLite, meta ≥0,90× sin alcanzar. Aceptación funcional completa verde. Faltan repeticiones de sweeps, estados y escalas de la matriz, ambos esquemas, ingesta/memoria y kill -9 específico. Ver `benchmark-results/optimization-2026-09-19/iteration-02/README.md`; no confundir ratio de latencias con throughput. |
| T15 | En curso | README, formato/diseño y resultados parciales actualizados; cierre final depende de T14 y de verificar la evidencia de aceptación pendiente. No se crearon commits. |

Cada cierre debe registrar:

```text
Tarea:
Resultado y archivos cambiados:
Decisiones y diferencias justificadas respecto del plan:
Pruebas ejecutadas y resultado:
Mediciones y ubicación de resultados, si corresponden:
Limitaciones o fallos pendientes:
Próxima tarea habilitada:
```

## Instrucción para iniciar una sesión con GPT TERRA

> Ejecuta el plan de `optimizar-tareas-terra.md`. Lee primero `optimizar.md` y el
> registro de avance. Empieza por la primera tarea pendiente cuyas dependencias
> estén completas y continúa secuencialmente dentro del alcance principal.
> Valida cada tarea, registra evidencia y conserva los resultados negativos.
> No commitees, no modifiques bases del usuario y no amplíes el alcance a tareas
> condicionales. No des por alcanzadas metas de rendimiento sin mediciones nuevas.
> Si falta una herramienta o hay un bloqueo, documenta la causa y continúa solo
> con trabajo independiente; no marques la tarea como completa.
