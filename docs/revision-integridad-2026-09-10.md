# Revisión adicional de integridad de EliteSQL

Revisión de código: `d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c`.
Fecha de inicio: 2026-09-10, zona America/Lima.

**Conclusión: hay situaciones reproducibles que permiten dejar datos huérfanos,
claves primarias duplicadas, escrituras parciales y sobrescrituras indebidas.
También hay casos de daño físico que la recuperación acepta como una base
válida después de descartar datos confirmados.** Estas correcciones tienen
prioridad sobre el plan de optimización de rendimiento.

El motor cuenta con defensas importantes y pruebas de recuperación útiles,
pero los fallos encontrados están en la interacción entre subsistemas y en
algunos límites de las validaciones. No son todos fallos del mecanismo de
commit: se distinguen integridad relacional, atomicidad de sentencias,
recuperación, integridad de índices y herramientas de rescate.

Se utilizaron exclusivamente bases temporales. Las alteraciones de WAL e
índices y la eliminación de archivos se hicieron sobre fixtures de prueba,
con la base cerrada o después de detener su proceso. No se modificaron datos
del usuario ni código del motor.

## Resumen de resultados

Todos los escenarios de esta tabla se reprodujeron; I08 recoge además una
limitación documentada del alcance actual de `check`.

| ID | Prioridad | Situación | Efecto observado |
|---|---|---|---|
| I01 | P0 | Reemplazo de padre o cambio de FK durante una transacción | Hijos apuntando a una clave que ya no existe |
| I02 | P0 | DROP INDEX del índice que protege una PRIMARY KEY | Dos filas con el mismo id SQL |
| I03 | P0 | Error intermedio en INSERT IGNORE / ON CONFLICT | Primeras filas ya confirmadas pese al error de la sentencia |
| I04 | P0 | Error de INSERT/UPDATE dentro de transacción explícita | El COMMIT posterior confirma parte de la sentencia fallida |
| I05 | P0 | Un bit alterado en una clave de navegación del índice paginado | Lectura falsa de ausencia y sobrescritura mediante INSERT |
| I06 | P0 | CRC incorrecto en WAL intermedio o sucesor WAL desaparecido | Reapertura exitosa con pérdida de commits confirmados |
| I07 | P1 | Repair de tabla con `id int AUTO_INCREMENT` | Omite todas las filas válidas del ejemplo en el destino |
| I08 | P1 | Verificación estructural sin verificación lógica completa | `check` declara válidos estados con problemas anteriores |

P0 significa bloquear una entrega que prometa preservar estas garantías hasta
corregir el caso. P1 exige una corrección prioritaria en las herramientas y sus
contratos. La probabilidad de daño físico no se equipara a la de una operación
SQL normal: I05/I06 requieren daño de archivos; I01–I04 no lo requieren.

## I01 · Claves foráneas que sobreviven a la desaparición de su padre

Referencias: `db.rs:7420` (`Txn::update`), `db.rs:7499` (expansión de cascadas),
`db.rs:9464` (`validate_foreign_keys`). Rutas relativas a
`crates/elitesql-core/src/`.

Fixture:

```sql
CREATE TABLE parents (code int);
CREATE UNIQUE INDEX ON parents(code);
CREATE TABLE children (parent_code int REFERENCES parents(code));
INSERT INTO parents(id, code) VALUES ('p', 1);
INSERT INTO children(parent_code) VALUES (1);
```

Desde la API de transacciones:

```python
tx = db.transaction()
tx.delete('parents', 'p')
tx.insert('parents', {'id': 'p', 'code': 2})
tx.commit()  # Aceptado
```

Resultado: el padre tiene `code=2`; el hijo conserva `parent_code=1`.
El verificador offline responde `ok: database validates`.

La validación de padres sólo revisa cambios cuya operación final es un delete.
El DELETE+INSERT del mismo id termina siendo un put; desaparece la evidencia
del borrado que dispararía la validación/cascada. `Txn::update` impide cambiar
claves referenciadas, pero esta ruta evita ese control.

Segunda reproducción: preparar un UPDATE del padre de `code=1` a `2`, crear
después la tabla hija con la FK e insertar un hijo que apunta a `1`, y finalmente
confirmar el UPDATE pendiente. También deja un huérfano. El esquema de la tabla
padre sigue igual, por lo que la comparación de esquemas al commit no detecta
que cambiaron sus dependencias entrantes.

**Corrección:** validar, bajo el mutex de commit, el valor anterior y final de
toda clave referenciada que se modifique, no sólo tombstones. Consultar las
dependencias del catálogo actual. Rechazar cambios de clave no soportados o
aplicar la semántica definida de borrado/reinserción dentro de la misma
transacción. No depender exclusivamente de controles al preparar el UPDATE.

**Regresiones:** RESTRICT/CASCADE, FK sobre id físico y columna única,
DELETE+INSERT del mismo id, padre reemplazado por otra fila, DDL intercalado,
escritor hijo concurrente y ciclos. Ningún estado final puede tener un hijo no
nulo sin padre.

## I02 · Es posible quitar la unicidad de una PRIMARY KEY

Referencias: `db.rs:4780`, `schema.rs:391`.

```sql
CREATE TABLE users (id int AUTO_INCREMENT PRIMARY KEY, name text);
INSERT INTO users(id, name) VALUES (1, 'a');
DROP INDEX ON users(id);
INSERT INTO users(id, name) VALUES (1, 'b');
```

Las cuatro sentencias son aceptadas. El SELECT devuelve `(1,'a')` y `(1,'b')`.
Los ids físicos internos son distintos, pero la PRIMARY KEY expuesta en SQL
deja de ser única.

La creación de tabla agrega un índice único para la identidad. DROP INDEX
protege índices usados por FK, pero no el índice que impone la identidad/PK;
la validación del esquema tampoco exige conservarlo.

**Corrección:** representar la restricción como invariante del esquema y
prohibir quitar su índice de soporte sin una operación explícita que modifique
la restricción. En el contrato actual las identidades son únicas: comprobarlo
también al abrir y validar catálogos.

**Regresiones:** DROP INDEX sobre PK/identidad rechaza la operación; inserción
explícita duplicada falla antes y después de checkpoint/reapertura. Un catálogo
inconsistente no debe habilitar escrituras silenciosamente.

## I03 · INSERT IGNORE confirma fila por fila

Referencia: `sql/exec.rs:4395`, `exec_insert_ignore_unique()`.

```sql
CREATE TABLE items (n int NOT NULL);
INSERT IGNORE INTO items(id,n) VALUES ('a',1), ('b',NULL);
```

La sentencia devuelve un error de NOT NULL, pero la fila `('a',1)` queda
confirmada. El ejecutor llama a `exec_insert()` por cada fila, y cada llamada
crea/confirma su propia transacción. La misma función atiende `ON CONFLICT DO
NOTHING`; se verificó esa ruta en código.

Esto también permite que otros lectores vean prefijos de una misma sentencia y
que un fallo de proceso deje un prefijo de sus filas. Contradice la promesa
general de INSERT múltiple atómico en `manual.md:35` y `manual.md:375`.

**Corrección:** una sola transacción por sentencia; descartar únicamente las
filas con el conflicto de unicidad autorizado, validar las demás en la vista
final y confirmar una vez. Errores de tipo, NOT NULL, FK o almacenamiento deben
abortar la sentencia completa. Mantener el orden y significado de RETURNING.

**Regresiones:** duplicados mezclados con filas válidas, error no suprimible en
la segunda/última fila, conflicto concurrente y kill -9 durante la ejecución.
La atomicidad debe observarse desde otra conexión y después de reapertura.

## I04 · Sentencia fallida deja cambios confirmables en una transacción

Referencias: `sql/exec.rs:446`, `sql/exec.rs:4460`, `sql/exec.rs:4764`.

Los ejecutores trabajan directamente sobre el staging de `Txn`, sin restaurar
el estado previo ni invalidar la transacción cuando una sentencia falla.

Dos reproducciones:

- Dentro de una transacción, insertar `('a',1),('b',NULL)` en una columna
  NOT NULL, capturar el error y hacer COMMIT: se conserva `('a',1)`.
- Con `a=1`, `b=9223372036854775807`, ejecutar `UPDATE nums SET n=n+1` dentro
  de una transacción. Falla por overflow en `b`; el COMMIT posterior tiene éxito
  y deja `a=2`, con `b` sin modificar.

El commit publica atómicamente lo que quedó preparado. El fallo está en la
atomicidad de la **sentencia**, que dejó un prefijo en ese estado preparado.
El context manager Python hace rollback si la excepción sale del bloque;
capturarla dentro del bloque permite que su salida confirme ese prefijo.

**Corrección:** escoger y documentar un contrato: rollback de sentencia mediante
savepoint interno, o transacción abortada que rechace COMMIT hasta rollback.
La primera opción permite conservar correctamente sentencias previas exitosas;
la segunda es más restrictiva y puede ser un primer arreglo seguro.

**Regresiones:** errores de conversión, overflow, NOT NULL, memoria y duplicados
intermedios; comprobar tanto el estado visible dentro de la transacción como
el resultado de COMMIT y reapertura. No confundir huecos permitidos en una
secuencia de identidad con filas parcialmente escritas.

## I05 · Metadatos del índice fuera del checksum permiten sobrescribir datos

Referencias: `paged.rs:64`, `paged.rs:190`, `paged.rs:295`, `paged.rs:910`;
carga de índices en `db.rs:3590` y uso para detectar duplicados en `Txn::insert`.

El formato paginado V2 protege con CRC el header global y el payload de cada
página. Las claves first/last de navegación se escriben por separado, fuera del
CRC del payload. Sus comprobaciones de orden no acreditan que correspondan a
las claves reales de la página. El manifiesto de runs registra generación y
tamaño, pero no un checksum completo que detecte esta alteración.

Reproducción sobre una base con ids `r1`, `r2`, `r3`, seguida de checkpoint y
cierre:

1. Modificar un único bit de la clave inicial del primer bloque de
   `indexes/primary.pidx`: cambiar el último byte de `r1` a `r3` (`0x31 XOR 2`).
2. El payload de la página y su CRC permanecen intactos y válidos. La clave
   inicial sigue siendo menor o igual a la final, que era `r3`.
3. Reabrir: `WHERE id='r1'` devuelve cero filas; un escaneo devuelve las tres.
4. Ejecutar `INSERT ... VALUES ('r1',99)`: es aceptado, aunque `r1` ya existe.
5. Tras checkpoint y reapertura, `r1` tiene `99` en lugar del valor original `1`.

Aquí un índice derivado dañado termina autorizando una modificación canónica
incorrecta. `check` no detectó el daño. No se recalculó ni falsificó ningún CRC
para producirlo.

**Corrección:** proteger claves de límites, longitudes, offsets y directorio con
checksums y validaciones de consistencia con los datos. Comprobar los
metadatos usados para descartar candidatos antes de confiar en una respuesta de
«no existe». Si el índice no pasa, reconstruirlo desde datos canónicos antes de
habilitar commits; nunca convertir un error de índice en ausencia de fila.

**Regresiones:** mutaciones de un bit en cada región del formato, incluidos
first/last y directorio; comparar point lookup/scan y verificar rechazo de
duplicados. Revisar índices secundarios que usan el mismo formato. Una futura
versión protegida necesita invalidar/reconstruir los runs antiguos, no confiar
en que sus checksums previos cubrían estas regiones.

## I06 · Recuperación WAL que puede ocultar pérdida de datos

### CRC inválido en un commit intermedio

Referencias: `wal.rs:139`, `db.rs:3822`, `check.rs:186`.

En modo Safe, el sidecar confirmó tres inserciones, con versiones 1, 2 y 3.
Después de kill -9 se copió la base temporal y se alteró sólo un byte del CRC
del segundo commit. El tercer frame permanecía completo y con CRC correcto.

Resultados de la copia dañada:

- `check`: advertencia de «torn tail» en offset 46 y código de salida 0.
- Apertura normal/SELECT: éxito, pero sólo conserva la primera fila.
- El WAL se reduce de 138 a 46 bytes: se elimina también el tercer commit sano.
- `check` posterior: éxito sin advertencias.

El scanner agrupa cualquier error como final incompleto y `open` trunca de
inmediato. No distingue un write incompleto al final de corrupción en el medio.
Esto no prueba que un kill -9 por sí solo pierda commits Safe: la reproducción
incluye daño deliberado del CRC posterior al crash.

**Corrección:** distinguir estados del scanner (EOF limpio, frame incompleto,
checksum inválido, secuencia inválida). La apertura normal debe rechazar o
poner en cuarentena corrupción no demostrada como cola incompleta y conservar
el original; el rescate destructivo debe ser una operación explícita hacia un
destino nuevo. No recuperar automáticamente commits posteriores saltando uno
intermedio: podrían depender de él.

### Sucesor WAL requerido desaparecido

El checkpoint en segundo plano publica un WAL puente y deja los commits nuevos
en el sucesor (`db.rs:10130`). En otra reproducción, el manifest señalaba
`wal_id=2`, `000002.wal` estaba vacío y `000003.wal` contenía una segunda fila
confirmada. Tras cerrar la base y eliminar únicamente `000003.wal`, `check`
pasó y la apertura devolvió sólo la primera fila.

Se verifica la existencia del WAL ancla, pero el recorrido termina normalmente
si falta el siguiente archivo. El manifest no demuestra hasta dónde debía
existir la cadena. `check` sólo valida el ancla y describe sucesores existentes
como obsoletos, aunque pueden contener los commits recientes.

**Corrección:** registrar de forma durable la identidad y continuidad esperada
de los WAL activos/sucesores, con publicación ordenada antes de confirmar
escrituras en ellos. Compartir las reglas de recorrido y validación entre
`open`, `check`, restore y repair. Una ausencia dentro de una cadena requerida
debe producir corrupción explícita, no una base aparentemente más antigua.

**Regresiones de ambas variantes:** daño inicial/intermedio/final, CRC y
longitudes, WAL puente vacío, sucesor ausente/corrupto, fallo antes/después de
cada rename/fsync y commits confirmados durante checkpoints. Conservar evidencia
de qué fue confirmado fuera del proceso de la base.

## I07 · Repair confunde el id físico con una columna SQL declarada

Referencia: `repair.rs:183`.

Se creó una base sana con `id int AUTO_INCREMENT PRIMARY KEY` y dos filas, y
se ejecutó `repair` hacia un destino nuevo. El resultado fue:

```text
recovered records: 0
skipped: 2
identity column 'id' requires an int
```

El comando devolvió código 0; la tabla destino estaba vacía. La herramienta
sobrescribe incondicionalmente `record['id']` con el ULID físico textual antes
de reinsertar. Eso invalida una columna SQL `id` entera que se había decodificado
correctamente. **La fuente permanece intacta y las omisiones se reportan**;
el problema no es que esta pérdida se oculte en el texto del reporte.

**Corrección:** separar siempre id físico y columna id declarada al reconstruir
filas. Conservar las secuencias de identidad, coordinando con H04 de la primera
auditoría. Exponer un resultado de rescate parcial inequívoco para automatización
y verificar destino antes de publicarlo como recuperación satisfactoria.

**Regresiones:** repair de una base sana debe recuperar todas sus filas y
relaciones; id implícito/declarado, identidad con otro nombre, blobs, FK y
referencias propias que crucen lotes. En una base dañada, cada pérdida debe
atribuirse al daño o a una política explícita, no al cambio del tipo del id.

## I08 · «check OK» no acredita integridad lógica completa

Referencia: `check.rs:32`. Su documentación actual describe comprobaciones de
estructuras y checksums. No implementa una verificación completa de unicidad,
FK, concordancia entre índices y datos ni máximos históricos de identidad.

En estas pruebas devolvió éxito para el huérfano de I01, el índice alterado de
I05 y los WAL de I06. El caso de cola dañada genera una advertencia inicial,
pero el código de salida sigue siendo 0. No debe interpretarse el mensaje
`database validates` como prueba de todas las invariantes de una base relacional.

**Corrección:** agregar una comprobación profunda que reconstruya la vista
visible desde fuentes canónicas y valide restricciones, referencias,
tipos/NULL, identidad y correspondencia de índices. Distinguir estado íntegro,
daño recuperable con posible pérdida y error. Validar la cadena WAL completa;
no depender del índice que se quiere verificar para enumerar sus registros.

La prueba `corruption.rs:81` llamada
`random_byte_flips_never_panic_or_corrupt_silently` acepta cualquier apertura
exitosa si las filas supervivientes se pueden leer y su título sigue siendo
texto. No compara filas/valores con un modelo ni detecta una pérdida silenciosa
de filas. Fortalecer sus aserciones y añadir mutaciones sobre índices, que no
están entre sus archivos candidatos actuales.

## Límite adicional: snapshot no equivale a serialización

Se comprobó un caso de write skew: dos transacciones leen que dos médicos están
de guardia, cada una desactiva a un médico distinto y ambas confirman. El estado
final no deja ninguno de guardia. `Txn` valida conflictos en registros escritos;
no valida todo el conjunto de lecturas ni predicados.

Este comportamiento es compatible con el alcance actual de aislamiento y no se
clasifica aquí como fallo de la garantía documentada. Sí puede romper reglas de
negocio entre varias filas si la aplicación asume serialización. Documentar ese
límite y usar una fila común de control actualizada en cada transacción, con
reintento completo ante conflicto, o diseñar validación serializable si es un
requisito del producto. El remedio debe probar la regla de negocio, no sólo que
ambos commits terminan.

## Defensas revisadas que sí existen

- Comparación del esquema preparado con el actual y detección de conflictos de
  escritura antes del WAL en la ruta general de commit.
- Validación de unicidad y de las rutas habituales de FK bajo exclusión del
  commit; la carrera normal de insertar hijo mientras se borra padre está probada.
- Checksums de frames WAL, datos de páginas y entradas de segmentos; apertura
  normal rechaza segmentos canónicos con CRC incorrecto.
- Diferenciación de `CommitUnknown` tras fallos de sync y publicación incierta;
  bloqueo de nuevas escrituras ante determinadas publicaciones canónicas ambiguas.
- Publicación mediante temporales/rename, preservación de generaciones y de
  blobs referenciados por snapshots o commits pendientes.
- Destinos nuevos y fuente protegida durante restore/repair.

Estas defensas reducen riesgos; no compensan los casos reproducidos. Un checksum
de payload no protege metadatos excluidos, y una comprobación de FK sólo en
DELETE no protege todas las maneras de cambiar una clave referenciada.

## Orden revisado del trabajo

1. **Guardar regresiones y endurecer la aceptación.** Convertir I01–I07 en
   pruebas deterministas, con aserciones antes/después de reapertura. Corregir
   los oráculos de corrupción. Mantener la suite existente como control.
2. **Proteger las escrituras canónicas.** Resolver I05 e I01/I02; el contrato de
   comparación numérica H01–H03 de la primera revisión también es obligatorio.
   No habilitar escrituras sobre un índice que no pudo validarse.
3. **Restablecer atomicidad SQL.** Resolver I03/I04 y probar fallos a mitad de
   sentencia, visibilidad concurrente y callbacks que capturan errores.
4. **Endurecer recuperación y herramientas.** Resolver I06–I08 y H04 de backup;
   probar secuencias, cadenas WAL, repair/restore y diagnóstico profundo.
5. **Aceptar bajo inyección de fallos.** Matriz de checkpoints/DDL/compacción con
   snapshots retenidos, error de I/O, disco lleno y kill -9. Exigir que todo
   commit confirmado sea visible o que la base rechace explícitamente el daño;
   nunca dar éxito con una pérdida no declarada. En Balanced/Fast, separar las
   pérdidas admitidas por crash del sistema de corrupción/atomicidad.
6. **Retomar performance.** Medir el costo de validación y optimizar manteniendo
   estas invariantes. Reestimar el calendario previo: la protección del formato
   paginado y el registro durable de cadenas WAL amplían el alcance original.

El primer resultado entregable debe ser una suite de regresiones que falle en
el HEAD actual y pase con cada corrección. No posponer restricciones ni
atomicidad para conservar un número de throughput del benchmark.

## Verificación ejecutada

- Ocho suites de integración seleccionadas: **57 pruebas pasan** (`recovery`,
  `txn`, `sql_foreign_keys`, `ddl_crash`, `backup`, `derived_run_recovery`,
  `crash_kill`, `compaction`).
- Tests de biblioteca core: **50 pruebas pasan**, incluyendo fallos de sync,
  publicación incierta, blobs y coordinación de mantenimiento.
- Total de esta segunda pasada: **107 pruebas existentes**, cero fallos,
  cero ignoradas. Se añadieron las reproducciones manuales descritas, ejecutadas
  contra el binario/FFI construidos de este HEAD; no se incorporaron aún al motor.
- No se ejecutó una campaña nueva de miles de crashes ni se simuló una pérdida
  real de energía. Kill -9 y corrupción de archivos no prueban por sí solos el
  comportamiento del dispositivo de almacenamiento ante un corte eléctrico.

Esta ampliación complementa
[la auditoría y plan iniciales](auditoria-y-plan-2026-09-10.md) y prevalece en el
orden de prioridades. La implementación permanece sin cambios.
