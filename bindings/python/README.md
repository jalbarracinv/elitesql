# clawdb (Python)

Binding de Python para ClawDB.

- `ClawDB(path)`: embebido en el proceso via la C ABI (`libclawdb`). ctypes
  libera el GIL en cada llamada, asi que los threads paralelizan de verdad.
  Requiere `libclawdb` construida (`cargo build --release -p clawdb-ffi`);
  se localiza automaticamente dentro del repo o via `CLAWDB_LIB`.
- `SidecarClient(socket)`: cliente del modo sidecar
  (`clawdb serve <db> <socket>`) para despliegues multi-worker
  (gunicorn, uwsgi).

```python
from clawdb import ClawDB

with ClawDB("app.clawdb") as db:
    db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")
    db.create_text_index("notes", "body")
    db.create_vector_index("notes", "emb", quantized=True)
    db.query("INSERT INTO notes (body, emb) VALUES ('hola mundo', '[...]')")

    hits = db.search_hybrid("notes", text=("body", "hola"), vector=("emb", embedding))
    with db.snapshot() as snap:
        rows = snap.scan("notes")   # lectura estable mientras otros escriben
```

Build del wheel: `python -m build --wheel` en este directorio (requiere
`pip install build`). La `libclawdb` se distribuye por separado o via
`CLAWDB_LIB`.
