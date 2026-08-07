# Formato en disco

Una base ClawDB es una carpeta autocontenida. Todos los enteros son
little-endian. `format_version` actual: 2.

```text
app.clawdb/
  CLAWDB          # marker: "clawdb format_version=2\n"
  LOCK            # flock: exclusivo (escritor) o compartido (read-only)
  catalog.json    # tablas, columnas, indices (JSON atomico via tmp+rename)
  manifest        # puntero atomico al estado visible
  manifest.prev   # manifest anterior (fallback de recovery)
  wal/NNNNNN.wal  # commits durables desde el ultimo checkpoint
  segments/NNNNNN.seg   # datos inmutables (se crean en checkpoint/compaction)
  vectors/XXXXXXXX.vidx # grafos ANN persistidos (derivados, desechables)
  blobs/<ulid>.blob     # chunks de blobs grandes (out-of-line)
```

## manifest

`CLAWMANI` (8 bytes) + u32 crc32 del cuerpo + u32 longitud + cuerpo JSON:
`{format_version, committed_version, segments: [{id, len}], wal_id}`.
Publicacion: escribir `manifest.tmp` (fsync), rotar `manifest` →
`manifest.prev`, rename `manifest.tmp` → `manifest`, fsync del directorio.
Un crash entre los renames deja un `manifest.prev` valido.

## Segmentos (`segments/`)

Log append-only de versiones de registros. Cada entrada:

```text
u8   kind          1 = put, 2 = tombstone
u64  version       secuencia global de commit
u16  table_len     + nombre de tabla (utf8)
u16  id_len        + id (utf8, ULID o provisto)
u32  payload_len   + payload (registro codificado; vacio en tombstone)
u32  crc32         sobre todos los bytes anteriores de la entrada
```

El manifest registra la longitud valida (`len`); bytes posteriores se
ignoran. Compaction reescribe los segmentos conservando solo las versiones
visibles para el ultimo estado o para snapshots vivos.

## WAL (`wal/`)

Un registro por commit (multi-cambio, atomico):

```text
u64  commit_version
u32  change_count
por cambio: u8 kind, u16+tabla, u16+id, u32+payload
u32  crc32 del registro completo
```

Replay idempotente: los registros con version <= la marca del manifest se
saltan; una cola rota se trunca. En checkpoint el WAL rota (id+1) y el
anterior se borra una vez publicado el manifest.

## Payload de registro

Auto-descriptivo (sobrevive evolucion de esquema):

```text
u16 field_count
por campo: u16+nombre, valor etiquetado
```

Tags de valor: 0 null, 1 bool, 2 int64, 3 float64, 4 text(u32+utf8),
5 blob(u32+bytes), 6 timestamp(i64 us), 7 json(u32+utf8), 8 vector(u32
count + f32*count), 9 date(i32 dias), 10 time(i64 us), 11 blobref
(u16+nombre, u64 size, u32 crc — referencia a `blobs/<nombre>.blob`).

## Blobs (`blobs/`)

Valores blob >= `external_blob_threshold` (default 256 KiB) se escriben
fuera de linea ANTES del commit WAL que los referencia:
`CLAWBLOB` (8) + u32 crc + u64 len + contenido. Lectura totalmente validada.
GC en compaction: se borran los chunks no referenciados por ningun payload
superviviente (incluye huerfanos de commits cortados).

## Grafos ANN (`vectors/`)

`CLAWVIDX` + crc + longitud + cuerpo: identidad del indice (tabla, columna,
metrica, m, ef_construction, quantized), version de commit del dump, y el
grafo completo (vectores f32 o int8+escala, niveles y vecinos, tombstones).
Se escribe al cerrar la base y al compactar. Al abrir: si valida y no es
mas nuevo que el estado, se carga y se pone al dia con lo commiteado
despues; ante cualquier problema se reconstruye desde los datos canonicos.
Nombre de archivo: crc32 de "tabla\0columna" en hex.

## Politica de versionado

`format_version` va en el marker, el catalogo y el manifest; un numero
distinto rechaza el open con error claro. Los indices derivados (vidx)
tienen su propio numero de formato: una version vieja simplemente se
descarta y reconstruye.
