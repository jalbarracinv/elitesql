#!/usr/bin/env bash
# Build a platform wheel of the Python binding with `libelitesql` inside.
#
#   bash bindings/python/build_wheel.sh            # builds the engine too
#   ELITESQL_LIB=/path/libelitesql.so bash bindings/python/build_wheel.sh
#   ELITESQL_WHEEL_PLATFORM=manylinux_2_28_x86_64 bash bindings/python/build_wheel.sh
#
# Output: bindings/python/dist/elitesql-<version>-py3-none-<platform>.whl
# Requirements: python3 with the `build` and `wheel` packages
# (`pip install build wheel`; `build` fetches setuptools>=77 into an isolated
# environment), and the Rust toolchain unless ELITESQL_LIB is given.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
python="${PYTHON:-python3}"

case "$(uname -s)" in
  Darwin) lib_name=libelitesql.dylib ;;
  Linux) lib_name=libelitesql.so ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

if [ -z "${ELITESQL_LIB:-}" ]; then
  (cd "$repo" && cargo build --release --locked -p elitesql-ffi)
  ELITESQL_LIB="$repo/target/release/$lib_name"
fi
test -f "$ELITESQL_LIB" || { echo "missing $ELITESQL_LIB" >&2; exit 1; }

# Platform tag of the wheel. Linux builds should run inside a manylinux
# container and name that tag explicitly; macOS derives it from the
# deployment target of the current machine.
if [ -z "${ELITESQL_WHEEL_PLATFORM:-}" ]; then
  case "$(uname -s)" in
    Darwin)
      arch="$(uname -m)"
      [ "$arch" = arm64 ] && ELITESQL_WHEEL_PLATFORM=macosx_11_0_arm64 || ELITESQL_WHEEL_PLATFORM=macosx_10_15_x86_64 ;;
    Linux)
      ELITESQL_WHEEL_PLATFORM="linux_$(uname -m)" ;;
  esac
fi

package="$here/elitesql"
cp "$ELITESQL_LIB" "$package/$lib_name"
trap 'rm -f "$package/$lib_name"' EXIT

rm -rf "$here/dist" "$here/build"
(cd "$here" && "$python" -m build --wheel --outdir dist >/dev/null)
pure="$(ls "$here"/dist/elitesql-*-py3-none-any.whl)"
(cd "$here/dist" && "$python" -m wheel tags --remove --platform-tag "$ELITESQL_WHEEL_PLATFORM" "$(basename "$pure")" >/dev/null)
rm -rf "$here/build"
ls "$here"/dist/*.whl
