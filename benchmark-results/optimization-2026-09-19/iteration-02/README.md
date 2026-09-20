# Iteración 02: coste de OFFSET y salida JSON

2026-09-19, macOS ARM64, release, durabilidad `balanced`. Comparación contra
el motor al inicio de esta iteración, que ya tenía el índice compuesto.
No representa una comparación contra HEAD limpio ni contra los números de
otra sesión. No se modificaron durabilidad, presupuestos ni mezcla.

## Cambios

- OFFSET cuenta miembros vivos del índice sin leer registros cuando el prefijo
  satisface todos los predicados. Los filtros adicionales siguen evaluándose.
- La continuación residente busca directamente tupla e identidad mediante
  rangos B-tree; no vuelve a recorrer los pares anteriores en cada lote.
- El lote conserva el par completo, evita reconstruirlo y evita duplicar las
  versiones de registros para retener lectores de segmento.
- Cancelación también se comprueba mientras se fusionan entradas descartadas.
- Si aparece un commit entre lotes, el fallback reinicia con scan sobre el
  snapshot original. No consulta el índice de igualdad del estado más nuevo.
- La salida JSON construye directamente arrays y objetos a partir de los
  valores convertidos; evita volver a serializar y copiar ese árbol. Conserva
  el protocolo y los valores etiquetados.

## Paginación aislada

SQL real de `browse`, 20 filas, todas las categorías, 5 repeticiones de 500
consultas por página/motor, 100 consultas de calentamiento por bloque. Cada
proceso crea ambas bases con la misma semilla, preserva los índices originales
y añade `(category, price_cents)`. Datos publicados mediante checkpoint,
sin escrituras cronometradas. Orden de motores alternado entre repeticiones.
Las secuencias de precios se verifican en todas las páginas y categorías;
la SQL no especifica desempate por identidad.

Medianas de los costes medios por bloque, en µs; **no son p50/p95/p99 de
peticiones individuales**. Tiempos crudos, planes y hashes de bibliotecas en
`browse-{before,final}-{5k,50k}.json`.

| Productos | OFFSET | Antes | Después | SQLite después | Reducción EliteSQL |
|---:|---:|---:|---:|---:|---:|
| 5.000 | 0 | 30,44 | 27,14 | 15,19 | 10,8 % |
| 5.000 | 40 | 51,45 | 29,62 | 15,57 | 42,4 % |
| 5.000 | 80 | 72,01 | 32,64 | 17,01 | 54,7 % |
| 5.000 | 200 | 132,34 | 40,93 | 19,12 | 69,1 % |
| 50.000 | 0 | 33,25 | 29,62 | 16,29 | 10,9 % |
| 50.000 | 40 | 58,46 | 32,37 | 17,06 | 44,6 % |
| 50.000 | 80 | 81,96 | 35,15 | 18,26 | 57,1 % |
| 50.000 | 200 | 155,48 | 43,51 | 21,25 | 72,0 % |
| 50.000 | 1.000 | 718,81 | 99,62 | 39,80 | 86,1 % |

Con 5.000 productos, OFFSET 1.000 queda fuera de la categoría: los 193,05 →
32,95 µs corresponden a un resultado vacío, no a una página de 20 registros.
La mejora no convierte OFFSET en acceso constante: aún recorre entradas del
índice, aunque evita materializar los registros descartados.

Rangos entre bloques en OFFSET 80: 5.000 productos, 71,43–72,77 →
31,91–37,64 µs; 50.000 productos, 80,10–84,28 → 34,24–35,91 µs.

La compilación intermedia sin el cambio JSON está registrada en
`browse-after-5k.json`: OFFSET 80 daba 34,02 µs y primera página 28,46 µs.
La mayor parte de la ganancia precede al cambio JSON; no atribuirle toda la
mejora. Esta comparación intermedia es secuencial, no un ensayo aleatorizado.

## Mezcla completa, primera comparación

`full-before.json` / `full-after.json`: 20.000 cuentas, 5.000 productos,
60 muestras por operación, 20 para las pesadas, tres repeticiones. Las 16
operaciones y divisor 98,3 son idénticos. La métrica pondera la mediana de
cada operación; no mide throughput concurrente.

| Métrica | Antes | Después |
|---|---:|---:|
| EliteSQL full-v2 | 78,57 µs | 74,73 µs |
| SQLite full-v2, medido en cada corrida | 38,41 µs | 37,53 µs |
| Ratio de latencias EliteSQL/SQLite | 2,05× | 1,99× |
| Browse dentro de la mezcla | 59,1 µs | 37,2 µs |

Reducción global EliteSQL de 4,9 %. La referencia previa de 2,17× usaba
otra configuración y menos muestras: no es la línea base de esta tabla.
Hay dispersión y algunas operaciones suben: `session_check` 26,6 → 30,3 µs,
`add_to_cart` 50,0 → 53,0 µs. No se declara ausencia de regresiones a partir
de tres bloques cortos.

`recommend` usa HNSW en EliteSQL y un fallback por categoría en SQLite;
no es equivalencia algorítmica. Excluyéndola y renormalizando, el ratio pasa
de 2,00× a 1,94×. El desfase restante no desaparece.

### Ampliación a 200 muestras (60 pesadas)

`full-before-200.json` / `full-after-200.json`, tres repeticiones y el resto
de configuración idéntica. Browse confirma 56,5 → 33,1 µs, pero la media
global es **77,9 → 77,9 µs**: no confirma el descenso de la corrida corta.
SQLite mide 39,1 → 38,7 µs y el ratio 1,99× → 2,01×.
Carrito sube 49,1 → 65,6 µs y checkout 203,2 → 260,7 µs, con dispersión entre
bloques. Esta corrida por sí sola no confirma una mejora global.

Se repitió en orden inverso: primero el motor nuevo y luego el anterior,
otra vez con tres repeticiones (`full-{after,before}-200-repeat.json`).
La nueva corrida da 75,11 → 68,11 µs, SQLite 36,92 → 36,85 µs. El pico de
checkout también aparece en el motor anterior: sus bloques combinados van
de 187,8 a 255,1 µs, frente a 185,5–267,6 µs del nuevo.

Como resumen exploratorio, reuniendo los **seis costes de bloque por operación**
de las dos invocaciones de cada versión y volviendo a ponderar sus medianas:

| Métrica agrupada | Antes | Después |
|---|---:|---:|
| EliteSQL full-v2 | 77,13 µs | 72,84 µs |
| SQLite full-v2 | 38,08 µs | 37,75 µs |
| Ratio de latencias | 2,03× | 1,93× |
| Browse | 53,48 µs | 31,84 µs |
| Carrito | 49,01 µs | 56,21 µs |
| Checkout | 225,81 µs | 227,74 µs |

La reducción agregada es 5,6 %, con variación importante entre invocaciones.
No es un intervalo de confianza ni garantiza ese porcentaje en producción.
No ocultar el carrito más lento en esta agregación. `full-pooled-200.json`
conserva los bloques, medianas y método. No se mezclan muestras de 60 con las
de 200 ni se presenta solo la mejor corrida (1,85×).

Se aislaron carrito y checkout con los mismos helpers de full-v2, 200 muestras
y seis repeticiones, base nueva por operación/repetición (`write_cost.py`).
La primera pareja da carrito 47,25 → 54,01 µs y checkout 208,70 → 204,32 µs.
Los bloques de carrito se solapan: 46,23–65,49 frente a 46,49–71,10 µs.
La dispersión persiste fuera de la mezcla. No se atribuye automáticamente al
cambio ni se declara ausencia de regresión de escritura.

Se repitió el control aislado en orden inverso. Esta vez carrito da
55,43 → 47,45 µs: el bloque lento también se presenta en el motor anterior.
Reuniendo los **12 bloques por operación/versión**, carrito queda en
47,45 → 48,04 µs (+1,2 %) y checkout 204,23 → 204,32 µs (+0,04 %);
SQLite permanece en 25,03 → 25,04 y 93,31 → 92,82 µs respectivamente.
Estos controles no reproducen una penalización estable del 14 % en carrito,
aunque tampoco certifican escrituras sostenidas o ingesta. Datos y método en
`write-{before,after}{,-repeat}.json` y `write-pooled.json`.

## Concurrencia exploratoria

Una corrida por motor, 10/100/500 usuarios, 20 s medidos por nivel más 3 s
de rampa y 3 s de calentamiento, 20.000 cuentas y 5.000 productos, escenario
`compound-index` en ambos motores. EliteSQL sidecar frente a SQLite embebido
en los generadores, como en el harness existente. Las bases son nuevas para
cada motor y acumulan estado entre niveles. Estos datos miden el estado final,
no una diferencia antes/después del cambio.

| Usuarios | EliteSQL ops/s | SQLite ops/s | Ratio throughput | p95 Elite / SQLite ms | p99 Elite / SQLite ms |
|---:|---:|---:|---:|---:|---:|
| 10 | 13.477 | 27.922 | 0,483× | 2,174 / 1,293 | 6,776 / 3,836 |
| 100 | 14.818 | 25.363 | 0,584× | 22,300 / 3,845 | 44,374 / 64,349 |
| 500 | 14.401 | 23.677 | 0,608× | 115,959 / 10,348 | 199,322 / 667,085 |

La meta ≥0,90× no se alcanza en esta corrida. EliteSQL mejora p99 a 100/500,
pero tiene peor p95. No se intercambia una conclusión por la otra. Su tasa
de éxito es 100 %, 99,995 % y 99,853 %; 16 y 464 errores por agotamiento de
reintentos de conflicto en los niveles mayores. SQLite registra 100 %.
Todas las invariantes de negocio pasan en ambos motores.

Tras SIGTERM, EliteSQL tiene cero errores y 548.894 advertencias de índices
derivados pendientes; tras abrir/cerrar, cero errores y cero advertencias.
SQLite pasa `integrity_check`. Esto prueba recuperación tras SIGTERM, no el
ensayo específico de `kill -9` durante rebuild requerido para cerrar T14.
Configuraciones, estadísticas, percentiles por operación y checks están en
`sweep-sidecar/` y `sweep-sqlite/`. Una sola corrida corta no aporta rangos
entre repeticiones ni certifica estabilidad sostenida.

## Reproducción

Desde la raíz, ejecutar secuencialmente con `ELITESQL_LIB` apuntando al archivo
exacto conservado antes o al archivo release final:

```bash
python3 examples/saas_simulation/browse_cost.py \
  --products 5000 --iterations 500 --repetitions 5 --out RESULTADO.json
python3 examples/saas_simulation/browse_cost.py \
  --products 50000 --iterations 500 --repetitions 5 --out RESULTADO.json
python3 examples/saas_simulation/ops_cost.py \
  --products 5000 --users 20000 --iterations 60 --heavy-iterations 20 \
  --repetitions 3 --scenario compound-index --out RESULTADO.json
python3 examples/saas_simulation/sweep.py --transport sidecar \
  --elitesql-bin "$PWD/target/release/elitesql" --levels 10,100,500 \
  --duration 20 --warmup 3 --ramp 3 --products 5000 --accounts 20000 \
  --scenario compound-index --out DIRECTORIO_RESULTADOS
# Repetir con --transport sqlite y un directorio distinto.
python3 benchmark-results/optimization-2026-09-19/iteration-02/write_cost.py \
  --out RESULTADO_ESCRITURAS.json
```

Para la ampliación full-v2, usar `--iterations 200 --heavy-iterations 60`;
correr antes/después y después/antes, cada invocación con tres repeticiones.
El control aislado de escrituras fija seis repeticiones internamente y se
ejecuta también en ambos órdenes. Sus conexiones se cierran y sus bases
temporales se eliminan al terminar.

`before.diff`, `before-secondary.rs` y `before-composite_secondary.rs`
conservan el estado anterior pertinente; las bibliotecas y CLI locales son
artefactos de medición, no archivos para publicar. No ejecutar mediciones
mientras corren compilaciones, pruebas u otros benchmarks.

## Validación y aceptación pendiente

`bash scripts/acceptance.sh` terminó con código 0. Incluye fmt, clippy estricto,
tests del workspace, compilación debug, 12 pruebas Python sin skips, Node
unitario/integración y sort con límite de 128 descriptores. El log registra
476 ejecuciones Rust correctas, contando la repetición específica del sort.
Python emitió un `ResourceWarning` de socket sin cerrar en su suite; no hubo
fallos. Evidencia completa en `acceptance.log`.

Las pruebas nuevas cubren OFFSET a través de lotes de tres filas, NULL,
empates, filtros adicionales, actualizaciones, borrados, compactación y
reapertura; un registro descartado de 8 KiB bajo presupuesto de 4 KiB;
commit intermedio con snapshot anterior; y compatibilidad JSON de valores
etiquetados, filas vacías y orden de columnas.

T14/T15 siguen abiertos. Faltan la matriz completa de estados residente/
publicado, repeticiones de concurrencia y comparaciones de escritura, ingesta
y memoria con ambos esquemas. No se declara alcanzada paridad con SQLite.

La primera medición de mezcla sitúa después de estos cambios el dashboard en
6,33 µs de la brecha ponderada, checkout en 5,10 µs y búsqueda de texto en
4,69 µs; browse queda en 3,42 µs. El siguiente análisis debe separar las tres
consultas del dashboard (GROUP BY, ingresos y stock bajo), perfilar el coste
de agregación/decodificación, y contrastarlo con checkout antes de elegir el
siguiente refactor. Son prioridades de investigación, no ganancias prometidas.
