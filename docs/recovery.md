# Guia de recovery

Principio de diseno: `After a crash, the database opens to the last fully
committed state.` Un commit es visible completo o no es visible. Y la regla
de oro: `Data files are canonical. Indexes are disposable.`

## Que pasa en un crash

ClawDB asume que el proceso puede morir en cualquier instruccion. El commit
escribe: (1) chunks de blobs grandes (fsync), (2) el registro WAL con CRC
(fsync segun modo de durabilidad), (3) aplica en memoria. El manifest solo
se reemplaza via archivo temporal + rename atomico, guardando el anterior
como `manifest.prev`.

Al abrir despues de un crash:

1. Se lee `manifest`; si su checksum falla, se usa `manifest.prev` (y el
   primario se re-establece sin jamas pisar la copia buena).
2. Se cargan los segmentos listados (CRC por entrada).
3. Se reaplica el WAL desde la marca del manifest — replay idempotente; una
   cola rota (commit a medio escribir) se trunca: ese commit nunca existio,
   completo o nada.
4. Los indices (secundarios, texto, ANN) se reconstruyen desde los datos
   canonicos; el grafo ANN se carga de su dump si es valido y se pone al dia,
   o se reconstruye.

Esto esta verificado con crash injection real: procesos asesinados con
`kill -9` en momentos aleatorios, miles de rondas, cero perdidas de commits
confirmados y cero commits parciales visibles.

## Herramientas, en orden de escalada

### 1. `clawdb check <db>` — diagnostico

Validacion offline de checksums y estructura: manifest y fallback, entradas
de segmentos, registros WAL, chunks de blobs referenciados, archivos
huerfanos. No modifica nada. Codigo de salida != 0 si hay errores.

### 2. `--read-only` — inspeccion de una base danada

Si `open` normal rechaza la base (corrupcion en un segmento listado), el modo
read-only abre igual: expone el prefijo valido de cada archivo, no escribe ni
un byte (lock compartido, sin truncar WAL, sin heal, sin dumps) y rechaza toda
escritura con `ReadOnly` (codigo 13). Util para mirar y exportar antes de
reparar:

```bash
clawdb export danada.clawdb tabla --read-only > rescate.jsonl
```

### 3. `clawdb repair <src> <dst>` — salvage

Reconstruye una base NUEVA en `dst` con todo lo recuperable de `src`:
recorre segmentos y WAL directamente (prefijo valido de cada archivo),
toma la ultima version de cada registro, respeta tombstones y re-inserta
con el esquema del catalogo. Requiere `catalog.json` legible.

Nunca es silencioso — el reporte cuenta recuperados, borrados legitimos,
saltados, y una nota por cada dano encontrado. Lo que quedo despues de un
punto de corrupcion en un archivo se pierde (y se reporta cuanto).

## Semantica por modo de durabilidad

- `safe`: un commit confirmado sobrevive crash del proceso Y del SO.
- `balanced`: sobrevive crash del proceso; un crash del SO puede perder los
  ultimos ~25ms de commits.
- `fast`: sobrevive crash del proceso (el WAL se escribio, el page cache lo
  tiene); un crash del SO puede perder los commits desde el ultimo checkpoint.

En los tres modos la atomicidad se mantiene: nunca medio commit.

## Que NO se repara solo

- `catalog.json` corrupto: el salvage necesita el esquema para re-tipar; hay
  que restaurarlo de un backup (es un JSON chico y estable — respaldalo).
- Chunks de blobs danados: el registro que los referencia falla su lectura
  con error explicito y check() lo reporta; el resto de la base sigue.
- Bit rot en un segmento en medio de datos viejos: open falla, read-only y
  repair aplican (prefijo valido).
