# Iteración 06: benchmark SaaS rehecho contra la base original

Continuación de las iteraciones [04](../iteration-04/README.md) y
[05](../iteration-05/README.md). Aquí se vuelve a medir todo lo acumulado
desde el commit `92e0290` (antes de la iteración 04) con el simulador SaaS
completo, hasta 2000 usuarios, e incluyendo a SQLite como referencia.

Fecha: 2026-09-25. Máquina: macOS arm64, 10 núcleos (4 de rendimiento y 6 de
eficiencia), con carga de fondo (navegador y escritorio). El generador de
carga en Python corre en la misma máquina y usa unos 2 núcleos.

## Método

- **Variantes:**
  - `base`: `92e0290`, compilado en un worktree aparte.
  - `current`: el árbol de trabajo del 2026-09-25, con las sondas temporales
    quitadas.
  - `sqlite`: el mismo simulador con `--transport sqlite`.

  Los hashes de los binarios están en `build-identities.txt`.
- **Etapas:** sweep encadenado 10 → 100 → 500 → 1000 → 2000 usuarios, con una
  sola base de datos por corrida, 30 s medidos por etapa, durabilidad
  `balanced` y sin pausa entre peticiones.
- **Repeticiones:** dos rondas intercaladas base / current / sqlite
  (`run.sh`). El resumen (`summarize.py` → `summary.txt`) promedia las dos
  rondas.
- **Métrica de eficiencia:** `ops/core-s` son las operaciones por segundo
  divididas por los núcleos de CPU del servidor. Es la comparación que menos
  depende del ruido de fondo, porque el servidor comparte la CPU con el
  generador.

## Resultado

| usuarios | base ops/s | current ops/s | Δ | p99 base → current | éxito base → current | current / SQLite |
|---:|---:|---:|---:|---:|---:|---:|
| 10 | 13 886 | 16 289 | +17 % | 6,0 → 4,8 ms | 100 → 100 % | 0,78× |
| 100 | 16 428 | 19 695 | +20 % | 41,0 → 42,1 ms | 99,999 → 100 % | 1,12× |
| 500 | 11 126 | 16 827 | +51 % | 264 → 141 ms | 99,76 → 100 % | 1,14× |
| 1000 | 9 569 | 16 767 | +75 % | 614 → 323 ms | 99,78 → 100 % | 1,16× |
| 2000 | 7 793 | 14 017 | +80 % | 5 005 → 573 ms | 98,92 → 100 % | 1,03× |

Eficiencia del servidor a 2000 usuarios: de 966 a 2 526 ops/core-s (2,6×).

**Errores.**

- `base`, por etapa desde 500 usuarios:
  - entre 388 y 1 055 operaciones agotaron sus reintentos por conflicto;
  - a 1000 usuarios hubo entre 157 y 239 rechazos de admisión de memoria
    (`error:16`), y a 2000, entre 2 859 y 3 001.
- `current`: cero de ambos en todos los niveles y en las dos rondas.

**Integridad.** La verificación offline sale bien en todas las corridas. Tras
matar el servidor sin cierre limpio (SIGTERM), `elitesql check` informa
advertencias sobre índices derivados que se reconstruyen al abrir: 1,10 M en
`base` y 1,51 M en `current`. Tras una apertura limpia quedan 0 advertencias y
0 errores. El número crece con el tamaño de la base: 130 MiB en `base` contra
216 MiB en `current`, porque `current` procesa más operaciones en los mismos
segundos. Por lo mismo, la reapertura tarda 21,5 s en `base` y 36,6 s en
`current`, con el mismo costo por MiB.

**Límites de la comparación.**

- El sweep encadenado favorece a `base`: `current` llega a los niveles altos
  con una base de datos más grande.
- SQLite sigue siendo más rápido con 10 usuarios.
- A 2000 usuarios, con cero pausa, la latencia media la fija la ley de Little
  (2000 / throughput); por eso el p50 no baja aunque el throughput suba.

## Cambios incluidos desde `92e0290`

Las iteraciones 04 y 05 están documentadas en sus propios README. Lo agregado
después, en la sesión del 2026-09-25:

1. **Reservas de memoria de consulta de 2× lo retenido.** Reemplazan los
   tamaños fijos por sentencia: las lecturas por igualdad y las sentencias de
   una fila reservan el doble de lo que miden. Esto elimina `error:16`.
2. **Vista de lectura publicada por RCU (`ReadView`).**
   - Las lecturas no toman el lock de estado.
   - Los índices primario y secundario usan estructuras copy-on-write propias
     (`PrimaryTableDelta` por bloques, `CowMap` y `CowSet`).
   - Los índices de texto y vectoriales tienen su propio lock.
   - La caché de parseo usa `RwLock<Arc<Statement>>`.
   - La versión de snapshot se publica después de la vista, así que una
     lectura nueva nunca pide una versión que la vista aún no cubre.
3. **Escaneo en streaming para agregados.** Los agregados recorren la tabla
   sin lotes; el recorrido del directorio no copia ids y valida los ids como
   ASCII sin validación UTF-8 completa. `count(*)` aislado baja 44 %.
4. **Registro de snapshots en 16 particiones**, en lugar de un único mutex
   global.
5. **Cerca de páginas por run.** Los runs primarios guardan el rango de
   páginas de cada tabla y los primeros 8 bytes del id como entero; la
   búsqueda de página compara enteros. En aislado, browse baja 15 % y el
   conteo por categoría 21 %.
6. **Decodificación de texto con vía rápida ASCII.**
7. **Materialización tardía en `ORDER BY … LIMIT` por igualdad.** Solo se
   decodifican completas las filas devueltas, desde el mismo payload. Con
   proyecciones anchas baja 9 %.
8. **Lotes del coordinador de commit.**
   - Un miembro que choca con otro o necesita replay ya no hace caer el lote:
     se aparta y se commitea solo, y el delta se reproduce dentro del lote.
   - El mantenimiento de índices derivados y la contabilidad de obsoletos se
     comparten con el camino individual.
   - La puerta para tablas con índice de texto o vector sigue cerrada: abrirla
     midió −9 %.

## Experimentos descartados en esta sesión

A/B de 2000 usuarios, intercalados. Todos se revirtieron.

| Experimento | Efecto medido |
|---|---|
| Lotes de hasta 256 miembros | sin efecto |
| Todo commit por el camino directo | −18 % |
| Todo commit por el lote, incluidas las tablas con texto o vector | −9 % |
| Mutex de commit sin traspaso justo | sin efecto |
| QoS alta para el líder o para quien retiene el mutex | sin efecto |
| Promoción temprana del siguiente líder | sin efecto |
| Control de admisión en el servidor (32 a 256 peticiones activas) | −23 % |

Hallazgo principal: a 2000 usuarios el límite es la CPU de la máquina, no el
pipeline de commit. La capacidad de commit se mantuvo en unos 7 550 commits/s
en todas las variantes del pipeline.

## Pendiente

- **Test intermitente de fusiones vectoriales** (resuelto el 2026-09-26,
  después de este benchmark; el binario `current` no lo incluye). No era una
  carrera ni pérdida de datos. Los ids "perdidos" seguían en el índice, pero
  eran inalcanzables: el test repite 50 veces un mismo vector, la regla de
  diversidad de HNSW aceptaba todas las copias y la poda desempataba por orden
  de inserción. Así, las últimas copias quedaban sin enlaces entrantes. Se
  corrigió desempatando por grado de entrada en la capa 0 y limitando los
  vecinos a distancia cero. Una prueba unitaria nueva falla con el código
  anterior; el test intermitente pasó 80 de 80 veces.
- **Tiempo de recuperación tras una caída:** los índices derivados se
  reconstruyen al abrir (36 s para 216 MiB). Persistirlos en cada checkpoint
  lo acortaría.
