# T14 — índices compuestos y lectura ordenada

Continuación: [iteración 02](iteration-02/README.md), con optimización de OFFSET,
mayores muestras y nueva validación. Las cifras de este documento describen
la medición inicial; no son el resultado final de las iteraciones siguientes.

Fecha: 2026-09-19. Código medido: árbol de trabajo de esta ejecución, con
`ESQLSID4`, catálogo de listas de columnas y recorrido `INDEX ORDERED`.

## Comandos

```bash
cargo build --release --workspace --locked
cargo run --release -p elitesql-core --example statement_cost
ELITESQL_LIB="$PWD/target/release" python3 examples/saas_simulation/ops_cost.py \
  --products 5000 --users 100 --iterations 10 --heavy-iterations 3 \
  --repetitions 3 --scenario baseline
ELITESQL_LIB="$PWD/target/release" python3 examples/saas_simulation/ops_cost.py \
  --products 5000 --users 100 --iterations 10 --heavy-iterations 3 \
  --repetitions 3 --scenario compound-index
```

Los datos brutos están en `T14-statement-cost.txt`,
`T14-ops-cost-baseline.{txt,json}` y `T14-ops-cost-compound.{txt,json}`.

## Resultados

`statement_cost` mide el motor aislado, con 5.000 productos y una página de
20 filas con offset 40 después de checkpoint:

| Escenario | EliteSQL motor |
|---|---:|
| índice `category` + sort | 133,67 µs |
| `category, price_cents` + `INDEX ORDERED` | 30,86 µs |
| diferencia | −102,8 µs (−76,9 %) |

El benchmark de operaciones usa 5.000 productos, 100 cuentas, tres
repeticiones, diez muestras por operación normal y tres por operación pesada.
Es una muestra de validación corta, no sustituye el sweep de concurrencia.

| Métrica | Baseline | Compound-index |
|---|---:|---:|
| browse EliteSQL | 215,1 µs [213,0, 232,3] | 75,5 µs [74,9, 92,1] |
| browse SQLite | 108,9 µs [102,2, 139,8] | 24,5 µs [24,5, 24,6] |
| mezcla full-v2 EliteSQL | 115,7 µs | 89,1 µs |
| mezcla full-v2 SQLite | 56,7 µs | 41,0 µs |
| ratio EliteSQL/SQLite | 2,04× | 2,17× |

Las muestras muestran una reducción de latencia de browse. No contienen
contadores de entradas visitadas que permitan cuantificar el trabajo evitado.
El ratio 2,17× corresponde a latencia ponderada por operación sin concurrencia:
no valida ni refuta la puerta de throughput de al menos `0,90×` SQLite.
Esa puerta sigue sin evaluar para esta entrega. Tampoco se afirma haber
alcanzado menos de 60 µs para la métrica histórica: esta ejecución usa
`full-v2`, no el divisor histórico. Falta un sweep emparejado de concurrencia
(10/100/500), repetir las escalas y muestras previstas en T02/T14 y medir las
regresiones de escritura, ingesta y memoria. T14 permanece en curso.

## Revisión de la brecha

La media ponderada cae un 23,0 % en EliteSQL y un 27,6 % en SQLite. La mejora
absoluta de EliteSQL convive con un ratio relativo mayor. Según las medianas
guardadas, la diferencia es 48,10 µs por operación ponderada:

| Operación | Exceso ponderado EliteSQL − SQLite | Parte de la diferencia |
|---|---:|---:|
| browse | 10,39 µs | 21,6 % |
| recommend | 9,45 µs | 19,6 % |
| admin_dashboard | 6,66 µs | 13,8 % |
| checkout | 6,01 µs | 12,5 % |
| search_text | 5,47 µs | 11,4 % |

`recommend` hace ANN en EliteSQL y una selección por categoría/ventas en
SQLite: no es una comparación del mismo algoritmo. Excluirla y renormalizar
las otras quince operaciones deja 79,96 frente a 38,34 µs (2,09×); la brecha
persiste. Esta descomposición orienta el perfilado y no sustituye una medición
con más muestras, calentamiento explícito y planes guardados.
