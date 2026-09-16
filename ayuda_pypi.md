# Publicar una nueva versión de `elitesql` en PyPI

La publicación es automática: al subir una etiqueta `vX.Y.Z` a GitHub, el
workflow `Wheels` (`.github/workflows/wheels.yml`) construye la biblioteca
nativa y las wheels, las prueba, las adjunta a la Release de GitHub y las sube
a PyPI. No hay tokens que guardar: PyPI confía en el workflow mediante
*trusted publishing* (OIDC), configurado el 2026-09-12 con el entorno `pypi`.

## Estado actual

- Proyecto en PyPI: https://pypi.org/project/elitesql/ (versión 0.0.1).
- Wheels publicadas por versión: Linux x86_64 y aarch64 (`manylinux_2_28`,
  glibc 2.28 o posterior) y macOS Apple Silicon (`macosx_11_0_arm64`).
  Todas llevan `libelitesql` dentro: `pip install elitesql` no necesita Rust.
- Python 3.9 o posterior.

## Flujo para publicar la versión X.Y.Z

1. **Actualizar la versión** en los dos sitios. Deben coincidir con la
   etiqueta que se va a crear:

   ```bash
   # bindings/python/pyproject.toml
   version = "X.Y.Z"
   # Cargo.toml (workspace.package)
   version = "X.Y.Z"
   ```

   Si `Cargo.toml` cambia, `Cargo.lock` cambia con él: ejecuta
   `cargo build --locked` o `cargo update -w` y añade el lock al commit.

2. **Verificar en local** antes de etiquetar:

   ```bash
   bash scripts/acceptance.sh                 # fmt, clippy, pruebas, Python, Node
   pip install build wheel
   bash bindings/python/build_wheel.sh        # produce bindings/python/dist/*.whl
   ```

3. **Confirmar y subir** el commit de versión a `main`:

   ```bash
   git add -A
   git commit -m "Release X.Y.Z"
   git push origin main
   ```

   Espera a que el workflow `Acceptance` de ese commit esté en verde.

4. **Crear y subir la etiqueta**. Esto dispara la publicación:

   ```bash
   git tag -a vX.Y.Z -m "EliteSQL X.Y.Z"
   git push origin vX.Y.Z
   ```

5. **Seguir el workflow** (unos 5 minutos):

   ```bash
   gh run list --workflow Wheels --limit 1
   gh run watch <id>
   ```

   O en la pestaña Actions del repositorio. Los jobs son: tres wheels
   (`wheel (x86_64, manylinux_2_28)`, `wheel (aarch64, manylinux_2_28)`,
   `wheel (macOS arm64)`), `Attach wheels to the GitHub Release` y
   `Publish to PyPI`.

6. **Comprobar** que la versión está disponible:

   ```bash
   curl -s https://pypi.org/pypi/elitesql/json | python3 -c "import sys,json; print(json.load(sys.stdin)['info']['version'])"
   python3 -m venv /tmp/prueba && /tmp/prueba/bin/pip install --no-cache-dir elitesql==X.Y.Z
   ```

PyPI no permite volver a subir una versión ya publicada, ni siquiera borrada:
si una release sale mal, se publica la siguiente (`X.Y.Z+1`).

## Publicar a mano una etiqueta ya existente

Si el job de PyPI no corrió para una etiqueta (por ejemplo, porque la variable
`PYPI_PUBLISH` no estaba puesta), se puede lanzar el workflow sobre esa
etiqueta con la publicación forzada:

```bash
gh workflow run wheels.yml --repo jalbarracinv/elitesql --ref vX.Y.Z -f publish_pypi=true
```

## Qué hay configurado y dónde

| Pieza | Dónde | Para qué |
|---|---|---|
| Pending/trusted publisher | PyPI → Your account → Publishing | Autoriza al workflow `wheels.yml` del repo `jalbarracinv/elitesql`, entorno `pypi`, a subir el proyecto `elitesql` |
| Entorno `pypi` | GitHub → Settings → Environments | Identidad OIDC que PyPI espera. Opcional: exigir aprobación manual antes de publicar |
| Variable `PYPI_PUBLISH=true` | GitHub → Settings → Secrets and variables → Actions → Variables | Habilita el job `Publish to PyPI` en cada etiqueta `v*`. Ponerla en `false` deja las wheels solo en la Release de GitHub |
| `bindings/python/pyproject.toml` | Repositorio | Metadatos y versión del paquete |
| `bindings/python/build_wheel.sh` | Repositorio | Construye la wheel con la biblioteca dentro; lo usa el workflow y sirve en local |

## Problemas frecuentes

- **`Publish to PyPI` falla con "invalid-publisher"**: los datos del publicador
  en PyPI no coinciden con el repositorio, el nombre del workflow o el entorno.
  Revisa la tabla anterior; debe ser exactamente `wheels.yml` y `pypi`.
- **"File already exists"**: esa versión ya está en PyPI. Sube la versión en
  `pyproject.toml` y etiqueta de nuevo. El job usa `skip-existing`, así que
  reintentar la misma etiqueta no rompe nada, pero tampoco sube nada.
- **La wheel de Linux no instala en una máquina antigua**: requiere glibc
  2.28 o posterior (Debian 10, Ubuntu 18.10, RHEL 8 o más nuevos). Para
  sistemas anteriores hay que compilar desde el código fuente.
- **Falta una plataforma (por ejemplo macOS Intel)**: añadir un job a la matriz
  de `wheels.yml` con el runner correspondiente y el tag de plataforma en
  `ELITESQL_WHEEL_PLATFORM`.

## Enlaces

- Wheels y notas por versión: https://github.com/jalbarracinv/elitesql/releases
- Workflows: https://github.com/jalbarracinv/elitesql/actions
- Documentación del paquete Python: `bindings/python/README.md`
