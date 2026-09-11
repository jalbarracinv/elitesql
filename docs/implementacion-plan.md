# Implementación del plan de integridad y rendimiento

Fecha: 2026-09-11. Base auditada: `d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c`.
Los cambios están en el árbol de trabajo. Este documento acompaña la
[auditoría](auditoria-y-plan-2026-09-10.md) y la
[revisión adicional de integridad](revision-integridad-2026-09-10.md).

## Cambios y evidencia

| Requisito | Implementación | Verificación |
|---|---|---|
| I01 · Integridad referencial | El commit revalida referencias entrantes ante reemplazos de padres y cambios de catálogo posteriores al staging | Reemplazo RESTRICT/CASCADE y creación intercalada de una FK; suites de FK, conflictos y DDL |
| I02 · Identidad/PK | Se protege su índice único; ADD COLUMN identity en tabla vacía lo crea y el catálogo verifica su presencia | DROP INDEX rechazado, identidad añadida conserva unicidad y catálogo inválido detectado |
| I03 · INSERT IGNORE | Una transacción y un commit por sentencia; sólo se omiten conflictos de unicidad; reintento completo bajo carrera | Duplicados, fallo posterior y escritores concurrentes; sin prefijos confirmados |
| I04 · Rollback de sentencia | Savepoint de staging con undo incremental y contabilidad de memoria | Error restaura staging, cascadas y sentencias anteriores; fallo de memoria no deja cambios parciales |
| I05 · Índices paginados | V3 protege cabeceras de página, límites y directorio con CRC; V1/V2 se verifican contra sus entradas | Alteración de cada byte de navegación, compatibilidad V1/V2 y reconstrucción antes de escribir |
| I06 · WAL | Preflight de cadena completa; extremo requerido durable en ambos manifests antes de cambiar escritor | Corrupción interior y sucesor final ausente rechazan apertura sin truncar; cola incompleta, fallos de publicación y kill -9 |
| I07 · Salvage | Clave física separada de columna SQL id; carga privada y validación FK antes de publicar destino | Copias con id declarado, máximo borrado y relaciones cíclicas que cruzan lotes |
| I08 · Check profundo | Imagen independiente desde segmentos/WAL, sort externo, restricciones y comparación con lecturas primarias/secundarias | Huérfanos, duplicados, secuencias inválidas y secundario incorrecto con CRC válido; fuzz compara con un prefijo confirmado exacto |
| H01/H02 · Números y joins | Comparación exacta int64, conversión a claves sólo sin pérdida y claves hash numéricas canónicas | Límites de i64/2^53; comparación diferencial con SQLite; INNER/LEFT/RIGHT, NULL, duplicados, índices y spill |
| H03 · Clientes int64 | Etiqueta decimal para enteros fuera del rango seguro; bigint en Node, int en Python, incluye identidades | Roundtrip real FFI/Python y sidecar/Node, consultas normales y cursores |
| H04 · Backup | Snapshot captura máximos de secuencia, incluso de filas eliminadas; restauración conserva claves físicas | Tabla vacía, máximo borrado, copia consistente con escritores y relaciones entre lotes |
| H05/H06 · Índices SQL | Driver compartido; prioridad id/único/secundario/scan independiente del orden AND; fallback MVCC si cambió el índice | Planes, resultados y cursor que mantiene su snapshot mientras cambian las filas coincidentes |
| H07 · Recursos de consulta | Admisión con plazo, reservas por batch, cancelación cooperativa, vencimiento remoto y cierre del iterador | Saturación, cancelación entre hilos, deadline, break, desconexión y backpressure Node; cola de 128 solicitudes/16 MiB |
| H08 · Ordenación | Top-k por heap y merge de hasta 32 runs por pasada; telemetría de buffers/cabeceras | Resultado y limpieza con spill bajo 128 descriptores; LIMIT/OFFSET y comparación con SQLite |
| H09 · Memoria | Transferencias sin truncar contadores; JSON anidado contabilizado; batches por bytes en scans, backup, restore y export | Filas anchas, fallo de staging antes de commit, admisión concurrente y mantenimiento |
| H10 · Frames | Límite de bytes JSON reales por batch; fila pendiente si no cabe | Textos que se expanden al escapar; reintento sin pérdida ni duplicación y error de fila individual |
| H11 · Commits | Instrumentación de asignaciones, preparación, lock, WAL, sync y apply; decisión de optimización sujeta a mediciones | Tres repeticiones por versión en 10K/20K/100K/1M; [informe y datos](../benchmark-results/review-2026-09-11/README.md) |
| H12 · Aceptación | scripts/acceptance.sh y CI Linux/macOS × Rust 1.89/1.93.1; benchmark registra SHA, patch, binarios, equipo y repeticiones | Aceptación local completa sin fallos; la matriz remota está definida y no se ha ejecutado aquí |
| Mantenibilidad | db/commit.rs, db/reads.rs, db/maintenance.rs; SQL values/planner/sort separados | Compilación, Clippy y suites sobre la nueva distribución de módulos |

## Validación

La puerta reproducible es `bash scripts/acceptance.sh`: formato, Clippy con
advertencias como errores, toda la suite Rust, build de workspace/FFI, pruebas
Python, pruebas Node con transporte simulado y sidecar real, y ordenación con
128 descriptores. La ejecución completa pasó con 408 pruebas Rust/doc, ocho Python, Node
unitario e integración real, además de la repetición bajo 128 descriptores.
Después de la última optimización del decodificador pasaron otras 41 pruebas
focalizadas, una nueva prueba de payload malformado, Clippy y build/roundtrips
de ambos clientes. [Log de validación](../benchmark-results/review-2026-09-11/validation.log).

Las regresiones focalizadas están en `tests/integrity_regressions.rs`,
`tests/numeric_differential.rs`, `tests/query_memory.rs`, las pruebas internas
de paged/check y las de CLI/sidecar. Las bases utilizadas son temporales.

La prueba concurrente de compactación requería una barrera de checkpoint antes
de esperar la cola: los commits pueden acabar antes de que el checkpoint
programe la compactación. Se agregó esa barrera y se mantiene la comprobación
de datos, compactación realizada y cero fallos de mantenimiento.

## Contratos y límites

- El nuevo formato V3 corresponde a índices reconstruibles; segmentos y WAL
  mantienen su formato canónico. Las pruebas cubren lectura V1/V2 y el nuevo V3.
- Un manifest antiguo sin `required_wal_id` no puede probar que un último WAL
  ya ausente existió antes de la actualización. No se recupera evidencia perdida.
- `check` requiere cerrar al escritor y espacio temporal proporcional a los
  datos. Sus parsers estructurales pueden materializar archivos y entradas;
  no se presenta como una operación online ni con RSS constante. Errores
  canónicos invalidan el reporte; inconsistencias derivadas son advertencias.
- El presupuesto es de admisión y memoria estimada, no un límite duro de RSS.
  Una fila muy ancha, buffers de transporte, mmap, resultados del llamador y
  overhead del allocator requieren memoria adicional. La fusión mínima de dos
  runs puede superar presupuestos diminutos; la telemetría lo estima.
- Cancelación cooperativa y plazos no interrumpen una publicación de commit
  iniciada. La API Rust permite cancelar desde otro hilo; el sidecar expone
  deadline y cierre del cursor, sin canal de cancelación fuera de banda.
- Los enteros etiquetados requieren actualizar consumidores JSON antiguos.
  Los números dentro de JSON de usuario conservan la semántica de JSON.
- Las pruebas reproducen los fallos identificados y diversos interleavings;
  no prueban exhaustivamente todo hardware, todos los fallos de disco ni todas
  las combinaciones de conexiones/cargas de la matriz propuesta.

## Rendimiento y cierre

Se completaron tres repeticiones alternadas de ambas versiones para 10K, 20K,
100K y 1M filas. En 20K, el cursor por id pasa de 4,887 ms a 4,62 µs y la
igualdad secundaria evita el scan. En 1M, la ordenación completa baja 7,7%,
con más I/O temporal por las pasadas acotadas. Top-k elimina temporales en
estas cargas, pero no acelera consistentemente el tiempo total. Los scans
cuestan aproximadamente 4–6% más; se registra esa regresión.

El profiling encontró una copia de nombre por columna añadida al validar
payloads: se eliminó sin quitar la detección de duplicados. Las asignaciones
por inserción simple/identidad/FK quedan al nivel de la base. No se introdujo
un arena ni se amplió el coordinador de commits sin evidencia que justificara
el riesgo. Se completó la evaluación experimental prevista, sin afirmar una
mejora de commits que las mediciones no demuestran.

El [informe de rendimiento](../benchmark-results/review-2026-09-11/README.md)
contiene variación entre repeticiones, p95/p99, RSS, asignaciones, spill,
contadores de commit, comandos, hardware, hashes y patches del código medido.
Las correcciones, pruebas, documentación y herramientas del plan están
implementadas localmente. La CI remota y la observación con cargas reales de
producción quedan como validación de despliegue; no se ha publicado ni hecho
commit/push de estos cambios.
