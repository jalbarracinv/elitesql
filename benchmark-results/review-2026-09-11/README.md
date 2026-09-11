# Verificación posterior a la implementación — 11 de septiembre de 2026

Comparación contra `d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c` (la revisión
auditada). El motor actual tiene cambios sin commit; cada directorio de
medición contiene `metadata.json`, el patch, hashes del código y de ambos
binarios, seis JSONL y los recursos de cada proceso. La etiqueta **actual**
se refiere a ese código capturado, no al SHA base sin su patch.

## Método

- Apple M5, 10 CPUs lógicas, 16 GiB; macOS 26.6.2 arm64, Rust 1.93.1.
- Alimentación AC, batería al 80% sin cargar. No se ejecutaron pruebas ni
  compilaciones en paralelo con las mediciones. No se controla toda actividad
  del sistema operativo ni la temperatura de la CPU.
- Mismo ejemplo `audit_performance.rs` en ambos motores. Base construida desde
  un `git archive` independiente; programa idéntico verificado por hash.
- Tres repeticiones por versión y tamaño, alternando el orden. Base nueva por
  proceso; caches del sistema calientes, sin expulsión. No es una prueba de I/O frío.
- Durabilidad **Fast** en ambos, workspace de consulta de 1 MiB, compactación
  automática deshabilitada y mismo contenido/orden de inserción determinista.
  Fast no mide fsync por commit; las garantías Safe se validan funcionalmente.
- 200 consultas por fase en 10K/20K; 30 en 100K y 10 en 1M. Top-k y sort usan
  10 operaciones por repetición, los perfiles de commit 200. Los p99 de fases
  con pocas muestras describen esas muestras, no un SLO fiable de cola.
- El contador global de asignaciones incluye todos los hilos y el driver; mide
  solicitudes de asignación, no memoria viva. Los tiempos incluyen el costo
  del contador atómico: son builds instrumentados comparables, no latencias
  de un binario sin instrumentación. RSS proviene de `/usr/bin/time -l`
  e incluye mmap y resultados entregados al llamador.
- Los resultados se verifican durante el benchmark. La carga no incluye red;
  los cursores invocan la ruta del core utilizada por el sidecar.

## Resultados

Las cifras siguientes son la **mediana de los p50 de tres repeticiones**,
no el p50 de muestras agrupadas. [summary.json](summary.json) incluye medianas
y rangos entre repeticiones de todas las métricas, incluidos p95/p99.

| Filas | Cursor por id: base → actual (µs) | Scan sin índice | Top-k | Sort completo | RSS base → actual (MiB) |
|---|---:|---:|---:|---:|---:|
| 10k | 2,397.88 → 3.75 | +4.9% | +1.2% | +4.4% | 13.4 → 13.1 |
| 20k | 4,887.04 → 4.62 | +4.3% | +2.0% | +4.2% | 20.3 → 20.0 |
| 100k | 24,157.88 → 5.29 | +5.9% | +4.6% | +2.0% | 69.5 → 72.3 |
| 1m | 242,640.50 → 5.08 | +5.1% | +1.9% | -7.7% | 579.9 → 601.0 |

En las columnas de variación, positivo significa **más lento**. RSS es el pico
de todo el proceso, incluyendo carga, índices, resultados completos y mmap.

Para 20K, el detalle por acceso muestra la diferencia entre corregir el plan y
acelerar trabajo que ya utilizaba un índice:

| Fase | Base p50 (µs) | Actual p50 (µs) | Variación |
|---|---:|---:|---:|
| `cursor_id` | 4,887.04 | 4.62 | -99.9% |
| `cursor_index` | 4,889.04 | 6.79 | -99.9% |
| `and_index_last` | 4,770.79 | 4.71 | -99.9% |
| `and_index_first` | 4.71 | 4.46 | -5.3% |
| `cursor_scan` | 5,073.62 | 5,292.33 | +4.3% |
| `top_k` | 5,631.79 | 5,747.08 | +2.0% |
| `full_sort` | 9,189.50 | 9,572.38 | +4.2% |
| `commit_plain` | 3.04 | 3.00 | -1.4% |
| `commit_identity` | 4.25 | 4.04 | -4.9% |
| `commit_foreign_key` | 6.42 | 6.46 | +0.6% |

`and_index_last` es una consulta buffered con una igualdad sin índice antes de
la igualdad indexada; `and_index_first` permuta los mismos predicados. El cursor
por índice y por id evita el scan que realizaba la versión auditada. El scan
con OR sigue recorriendo todas las filas. No se atribuyen los factores de mejora
de búsquedas selectivas a consultas no indexadas ni a latencia de red.

El costo medido de scan está entre +4% y +6%; top-k no presenta una mejora
consistente de latencia, pero elimina el spill en estas cargas. En 1M, el sort
completo mejora aproximadamente 7%, y escribe **760 MB** temporales por
repetición frente a **380 MB** en la base: la pasada intermedia acota los
descriptores a costa de I/O. El límite de 128 descriptores se verifica aparte.

Los picos de RSS de 1M superan 384 MiB en **ambas** versiones. El presupuesto
del gobernador no es un límite de RSS; el resultado materializado y las páginas
mapeadas explican parte de esa diferencia. Estas mediciones no aíslan cada
componente ni prueban un máximo universal de memoria.

## Profiling y decisiones

El primer experimento [20k/metadata.json](20k/metadata.json) detectó una
regresión de aproximadamente 10–15% en scans/sort. La detección de nombres
duplicados clonaba cada nombre de columna al decodificar. Usar la entrada del
mapa conserva la validación y elimina **8 millones de asignaciones** en las
200 consultas de scan de 20K. Las mediciones finales están en
[20k-final/metadata.json](20k-final/metadata.json); no se descartan los datos
iniciales desfavorables.

En 20K, los perfiles simple/identidad/FK registran aproximadamente
**46/73/90 asignaciones por inserción**, iguales a la base. La preparación bajo
lock del perfil FK ronda **2,7 µs por operación**, frente a unos **3 µs** del
contador WAL; el perfil simple prepara bajo lock alrededor de **0,08 µs**.
Los contadores incluyen operaciones anidadas y no deben sumarse como fases
mutuamente excluyentes. Fast no hace sync por commit.

No se introdujo un arena por commit ni se amplió el coordinador a identidades
y FK: estos datos no demuestran un ahorro dominante de asignaciones que
justifique tocar esas garantías. Tampoco se declara cumplida la meta
experimental de +10% en commits o un umbral p99 global. La corrección de
planificación sí demuestra una mejora repetible; el control de recursos tiene
costos que deben considerarse al dimensionar cargas de scan.

## Validación funcional

[validation.log](validation.log) conserva una ejecución completa de
`scripts/acceptance.sh`: 408 pruebas Rust/doc, ocho Python, Node unitario y con
sidecar real, más la repetición de sort con límite de 128 descriptores. Todos
terminaron sin fallos. Tras la última optimización del decodificador se repitieron
las 41 pruebas de integridad, diferencial SQL, corrupción y memoria, se agregó
y ejecutó una prueba de rechazo de columnas repetidas/bytes sobrantes, y se
repitieron Clippy, build de workspace/FFI, Python y ambos tests Node. La salida
normal de los tests Node es vacía; sus comandos finalizaron con código 0.

La CI Linux/macOS × Rust 1.89/1.93.1 está definida en el repositorio; no se
ha ejecutado remotamente en esta sesión. Este informe acredita la máquina local.

## Reproducción

Desde el árbol de código capturado en `source.patch`:

```bash
bash scripts/acceptance.sh
cargo build --release --locked -p elitesql-core --example audit_performance
bench_base=$(mktemp -d)
git archive d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c | tar -x -C "$bench_base"
cp crates/elitesql-core/examples/audit_performance.rs "$bench_base/crates/elitesql-core/examples/"
cargo build --manifest-path "$bench_base/Cargo.toml" --release --locked -p elitesql-core --example audit_performance
python3 scripts/review-benchmark.py \
  --output benchmark-results/local-review \
  --baseline-binary "$bench_base/target/release/examples/audit_performance" \
  --baseline-sha d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c \
  --rows 20000 --queries 200 --repetitions 3 \
  --power-note 'describir las condiciones observadas'
```

Usar un directorio de salida nuevo. Para repetir la escala, cambiar a
`--rows 10000 --queries 200`, `--rows 100000 --queries 30` y
`--rows 1000000 --queries 10`, manteniendo tres repeticiones. No ejecutar los
benchmarks junto con pruebas, compilaciones u otros benchmarks.
