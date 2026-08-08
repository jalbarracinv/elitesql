# Búsqueda vectorial con Potion Multilingual y MIRACL-es

Prueba reproducible de búsqueda semántica con:

- [`minishlab/potion-multilingual-128M`](https://huggingface.co/minishlab/potion-multilingual-128M), ejecutado con Model2Vec. Produce embeddings de 256 dimensiones.
- [`jinaai/miracl-es`](https://huggingface.co/datasets/jinaai/miracl-es), 648 consultas en español con pasajes positivos y negativos provenientes de MIRACL/Wikipedia.
- El índice HNSW con distancia coseno de EliteSQL.

Las revisiones del modelo y del dataset, además del SHA-256 del dataset, están fijadas en el script. La primera ejecución descarga aproximadamente 1.5 MB de datos y el modelo; las siguientes reutilizan las cachés.

## Ejecución

Desde la raíz del repositorio:

```bash
python3 -m venv .venv-potion
source .venv-potion/bin/activate
pip install -r examples/vector_search_potion/requirements.txt
cargo build --release -p elitesql-ffi

python examples/vector_search_potion/miracl_search.py --rebuild
```

Para una comprobación rápida:

```bash
python examples/vector_search_potion/miracl_search.py \
  --max-queries 50 \
  --db target/potion-miracl-es-50.esql \
  --rebuild
```

Para ampliar la prueba a 250.000 pasajes reales de MIRACL-es:

```bash
python examples/vector_search_potion/miracl_search.py \
  --corpus-size 250000 \
  --db target/potion-miracl-es-250k.esql \
  --total-memory-mib 640 \
  --maintenance-memory-mib 512 \
  --rebuild \
  --output-json benchmark-results/potion-miracl-es-250k.json
```

El script conserva primero todos los candidatos juzgados —incluidos los negativos difíciles— y completa el tamaño solicitado con el primer fragmento fijado del corpus oficial. Así todas las consultas mantienen sus documentos relevantes dentro del índice.

La configuración predeterminada de 128 MiB totales y 32 MiB de mantenimiento
rechaza de forma segura la construcción HNSW de 250K con `MemoryLimit`. Los
límites mayores del ejemplo son explícitos: no convierten el presupuesto
predeterminado en una promesa de que esta construcción cabe en 128 MiB.

Para guardar resultados comparables entre configuraciones:

```bash
python examples/vector_search_potion/miracl_search.py \
  --ef-search 256 \
  --output-json benchmark-results/potion-miracl-es-ef256.json
```

El corpus completo de MIRACL-es tiene más de 10 millones de pasajes. Por defecto, esta prueba usa el subconjunto público de *reranking* (6,400 pasajes únicos), suficientemente pequeño para ejecutarse localmente y con etiquetas de relevancia disponibles.

## Resultado de referencia con 250K

Ejecución local del 2026-08-08 con índice `float32`, `top_k=10`, presupuesto
total de 640 MiB y 512 MiB para mantenimiento:

```text
Embeddings de 250.000 pasajes    11,649 s
Carga en EliteSQL                55,895 s
Construcción HNSW              230,762 s
Proceso completo                305,760 s
Tamaño de la base               690,12 MiB
```

Comparación sobre el mismo índice persistido:

| `ef_search` | ANN recall@10 | Media | p95 | Hit@10 global |
|---:|---:|---:|---:|---:|
| 128 | 0,9630 | 1,341 ms | 1,598 ms | 0,5710 |
| 256 | 0,9789 | 1,721 ms | 2,106 ms | 0,5895 |
| 512 | 0,9880 | 2,486 ms | 3,173 ms | 0,5957 |

La referencia exacta obtiene `Hit@10=0,5972`, de modo que `ef_search=512` prácticamente elimina la pérdida atribuible al ANN. `256` ofrece el mejor equilibrio si importa más la latencia.

Las cifras de tiempo dependen del hardware. El JSON de construcción está en
`benchmark-results/potion-miracl-es-250k.json`; las búsquedas comparables en
procesos nuevos están en los archivos `-ef128`, `-ef256` y `-ef512` del mismo
directorio. [benchmark.md](../../benchmark.md) documenta el entorno y las
mediciones de memoria.

## Qué significan las métricas

- `Reranking MIRACL (exacto)`: calidad del embedding dentro del conjunto de candidatos positivos/negativos definido para cada consulta. Aísla el comportamiento del modelo.
- `Corpus global (exacto)`: búsqueda por fuerza bruta sobre la unión de todos los candidatos. Es una referencia para la búsqueda global; los pasajes no etiquetados para una consulta se consideran no relevantes.
- `Corpus global (EliteSQL)`: las mismas métricas usando HNSW en EliteSQL.
- `ANN recall@K`: solapamiento entre el top-K de EliteSQL y el top-K exacto. Aísla la fidelidad del índice aproximado.
- `Latencia EliteSQL`: tiempo de `search_vector`, sin incluir la generación del embedding de la consulta.

El conjunto `jinaai/miracl-es` fue preparado para *reranking*, no como un corpus global exhaustivamente juzgado. Por eso la comparación más limpia de calidad semántica es la primera línea; en las métricas de corpus global puede haber falsos negativos no etiquetados.

Opciones útiles:

```text
--top-k 10                 tamaño del ranking
--ef-search 128            haz de búsqueda HNSW
--quantized-index          índice int8 (~4x menos memoria)
--max-queries N            limita el dataset; 0 usa las 648 consultas
--corpus-size N            amplía el índice a N pasajes; 0 usa 6.400
--total-memory-mib N       presupuesto lógico total de EliteSQL
--maintenance-memory-mib N parte del presupuesto reservada a mantenimiento
--rebuild                  reconstruye la base generada
--output-json RUTA         exporta métricas estructuradas
```
