# Base de la optimización de índices compuestos

Fecha: 2026-09-19. Árbol inicial: `b4dda86`, con cambios sin commit solo en
la métrica `ops_cost`. Máquina: Apple Silicon arm64, macOS Darwin 25.6.0,
Python 3.11.13, Node 26.5.0 y Rust 1.89.0.

La máquina no tenía Rust al empezar. Se instaló el toolchain 1.89.0 indicado
por el README. Antes de modificar el motor pasaron:

```text
cargo fmt --all -- --check      ok
cargo test --workspace --locked ok
```

La validación incluyó recuperación, `kill -9`, DDL, snapshots, índices
secundarios, memoria, SQL, FFI y documentación. Clippy, clientes y el sweep
completo quedan para la aceptación final.

## Fixture de formato anterior

El binario anterior creó `/private/tmp/elitesqlsid3-fixture.esql`: una tabla
con identidad declarada, índice secundario `items(category)`, tres filas y
tiradas `ESQLSID3`. El CLI leyó las tres filas correctamente. Se conserva para
abrirlo después de cambiar el marcador; no se simulará la migración alterando
el marcador de un archivo recién creado.

## Motor solo, datos publicados

`cargo run --release -p elitesql-core --example statement_cost` produjo:

| medición | resultado |
|---|---:|
| `Db::get` sin SQL | 0,22 µs |
| statement SQL puntual | 1,28 µs |
| ABI JSON puntual | 1,95 µs |
| `browse` en motor, 339 leídas / 20 devueltas | 133,04 µs |
| `browse` por ABI JSON | 137,56 µs |
| fila en scan publicado | 77,7 ns |
| fila por índice, 40 filas | 341 ns |

## Mezcla `full-v2`

La métrica nueva normaliza los 16 pesos de `vuser.py` (suma 98,3), prepara una
identidad independiente por muestra y alterna motores. Se midió con:

```bash
ELITESQL_LIB="$PWD/target/release" python3 examples/saas_simulation/ops_cost.py \
  --products 5000 --users 20000 --iterations 60 --heavy-iterations 20 \
  --repetitions 3 --out /private/tmp/ops-cost-v2-baseline-5k.json

ELITESQL_LIB="$PWD/target/release" python3 examples/saas_simulation/ops_cost.py \
  --products 50000 --users 20000 --iterations 20 --heavy-iterations 10 \
  --repetitions 3 --out /private/tmp/ops-cost-v2-baseline-50k.json
```

| productos | EliteSQL | SQLite | ratio Elite/SQLite |
|---:|---:|---:|---:|
| 5.000 | 105,7 µs | 53,3 µs | 1,98× |
| 50.000 | 836,2 µs | 777,0 µs | 1,08× |

Estas cifras no son la meta histórica de 60 µs. La medición de 50.000 usa
menos muestras; antes de una afirmación final se repetirá con el sweep
emparejado. `recommend` conserva el workload, pero compara HNSW con un
fallback por categoría de SQLite, no dos consultas SQL equivalentes.

## Sweep concurrente, esquema original

Dos sweeps consecutivos de 60 segundos por nivel, con 5.000 productos y
20.000 cuentas, dieron estas referencias. Los reportes completos están en
`/private/tmp/elitesql-compound-baseline-sidecar` y
`/private/tmp/elitesql-compound-baseline-sqlite`.

| usuarios | EliteSQL ops/s | SQLite ops/s | ratio | EliteSQL p99 | SQLite p99 |
|---:|---:|---:|---:|---:|---:|
| 10 | 10.572,0 | 14.673,4 | 0,72× | 8,288 ms | 9,361 ms |
| 100 | 10.995,1 | 12.227,9 | 0,90× | 68,349 ms | 150,163 ms |
| 500 | 8.175,1 | 8.407,9 | 0,97× | 387,938 ms | 1.741,116 ms |

Los invariantes de negocio y read-your-writes fueron correctos en ambos. El
`check` de SQLite fue limpio. El de EliteSQL devolvió salida 3, cero errores y
963.560 advertencias de índices derivados antes y después de reapertura. Es
una regresión de observabilidad o ciclo de vida previa a los compuestos; no se
considera aceptación limpia y se repetirá al final.

Como referencia de escritura, `write_cost 2 2000` dio 12,5 y 6,2 µs por
autocommit insert en dos bloques de 2.000 filas, con 2,2 y 2,1 µs por lectura
puntual. El microcoste de commit de `statement_cost` fue 5,35 µs.
