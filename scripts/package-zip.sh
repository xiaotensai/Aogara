#!/usr/bin/env bash
#
# Package a portable zip for the given target triple.
#
# Layout:
#   dist/aogara-<version>-<os>-<arch>.zip
#     aogara-<version>-<os>-<arch>/
#       aogara[.exe]
#       runtime/python/<triple>/...
#       runtime/uv/<triple>/...
#       LICENSE.txt
#       NOTICE
#       README.txt            (from repository README.md, public/opensource version)
#       config.example.toml   (sanitized template)
#       THIRD_PARTY_NOTICES.txt
#
# Usage:
#   scripts/package-zip.sh <target-triple>
#   RUNTIME_ROOT="<安装目录>/app" scripts/package-zip.sh x86_64-pc-windows-msvc
#
# Runtime layout:
#   - GitHub CI / standard: scripts/fetch-runtime.sh downloads into <repo>/runtime/
#   - Local production:     runtime lives under <安装目录>/app/runtime/
#   Set RUNTIME_ROOT to override auto-detection (the value is the directory
#   CONTAINING `runtime/`, not runtime/ itself).
#
# Reads the package version from the first `version = "..."` line in
# Cargo.toml — that's the [package] version because [package] is the first table.

set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <target-triple>" >&2
  exit 2
fi

TARGET="$1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(grep -m1 '^version = ' "$ROOT/Cargo.toml" | cut -d'"' -f2)"

case "$TARGET" in
  x86_64-unknown-linux-gnu) OS=linux;   ARCH=x64;   EXE=aogara ;;
  aarch64-apple-darwin)     OS=macos;   ARCH=arm64; EXE=aogara ;;
  x86_64-pc-windows-msvc)   OS=windows; ARCH=x64;   EXE=aogara.exe ;;
  *)
    echo "unknown target triple: $TARGET" >&2
    exit 2
    ;;
esac

PKG="aogara-${VERSION}-${OS}-${ARCH}"
STAGE_ROOT="$ROOT/dist/staging"
STAGE="$STAGE_ROOT/$PKG"
ARCHIVE="$ROOT/dist/${PKG}.zip"

# Runtime root auto-detection: <dir>/runtime/{python,uv}/<triple>
if [[ -n "${RUNTIME_ROOT:-}" ]]; then
  RT="$RUNTIME_ROOT"
elif [[ -d "$ROOT/app/runtime/python/$TARGET" ]]; then
  RT="$ROOT/app"
else
  RT="$ROOT"
fi
PY_SRC="$RT/runtime/python/$TARGET"
UV_SRC="$RT/runtime/uv/$TARGET"
BIN_SRC="$ROOT/target/$TARGET/release/$EXE"

if [[ ! -f "$BIN_SRC" ]]; then
  echo "binary not found at $BIN_SRC — run cargo build / tauri build first" >&2
  exit 1
fi
if [[ ! -d "$PY_SRC" ]] || [[ ! -d "$UV_SRC" ]]; then
  echo "runtime tree missing under $RT/runtime — run scripts/fetch-runtime.sh $TARGET or set RUNTIME_ROOT" >&2
  exit 1
fi
for ASSET in README.md config.example.toml THIRD_PARTY_NOTICES.txt LICENSE.txt NOTICE; do
  if [[ ! -f "$ROOT/$ASSET" ]]; then
    echo "required asset missing: $ROOT/$ASSET" >&2
    exit 1
  fi
done

rm -rf "$STAGE"
mkdir -p "$STAGE/runtime/python" "$STAGE/runtime/uv"

cp "$BIN_SRC" "$STAGE/$EXE"
if [[ "$OS" != "windows" ]]; then
  chmod +x "$STAGE/$EXE"
fi

# -RP preserves symlinks (python-build-standalone uses internal symlinks
# like bin/python3.12 -> bin/python on linux/mac; without -P they get
# duplicated and the zip nearly doubles).
cp -RP "$PY_SRC" "$STAGE/runtime/python/$TARGET"
cp -RP "$UV_SRC" "$STAGE/runtime/uv/$TARGET"

cp "$ROOT/LICENSE.txt"              "$STAGE/LICENSE.txt"
cp "$ROOT/NOTICE"                   "$STAGE/NOTICE"
cp "$ROOT/README.md"                "$STAGE/README.txt"
cp "$ROOT/config.example.toml"      "$STAGE/config.example.toml"
cp "$ROOT/THIRD_PARTY_NOTICES.txt"  "$STAGE/THIRD_PARTY_NOTICES.txt"

# Fail-closed: no private data may enter the staging tree.
# certifi/cacert.pem is a public CA bundle shipped by pip and is whitelisted.
LEAK="$(find "$STAGE" \( \
    -name 'config.toml' -o -name '*.key' -o -name '*.pem' \
    -o -name 'accounts.json' -o -name '*.log' \
    -o -path '*/profiles/*' -o -path '*/logs/*' \
    -o -path '*/ca/*' -o -path '*/configs/*' -o -path '*/credentials/*' \
  \) -not -path '*/certifi/cacert.pem' -print)"
if [[ -n "$LEAK" ]]; then
  echo "ERROR: private data leaked into staging:" >&2
  echo "$LEAK" >&2
  exit 1
fi

mkdir -p "$ROOT/dist"
rm -f "$ARCHIVE"

# Pick a zip tool: windows-latest GHA runners ship 7z but not zip;
# linux/macos runners (and most user shells) ship the `zip` command.
# Local Windows Git Bash has neither, so fall back to Python's zipfile.
ZIP_TOOL=""
if command -v 7z >/dev/null 2>&1; then
  ZIP_TOOL=7z
elif command -v zip >/dev/null 2>&1; then
  ZIP_TOOL=zip
elif command -v python >/dev/null 2>&1; then
  ZIP_TOOL=python
else
  echo "no zip tool found (need 7z, zip, or python)" >&2
  exit 1
fi

case "$ZIP_TOOL" in
  7z)
    (cd "$STAGE_ROOT" && 7z a -tzip -mx=5 "$ARCHIVE" "$PKG/" >/dev/null)
    ;;
  zip)
    # -y preserves the symlinks inside the python-build-standalone tree.
    (cd "$STAGE_ROOT" && zip -r -y -9 "$ARCHIVE" "$PKG/" >/dev/null)
    ;;
  python)
    python - "$STAGE_ROOT" "$PKG" "$ARCHIVE" <<'PY'
import os, sys, zipfile
stage_root, pkg, archive = sys.argv[1], sys.argv[2], sys.argv[3]
base = os.path.join(stage_root, pkg)
count = 0
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as zf:
    for dirpath, dirnames, filenames in os.walk(base, followlinks=False):
        for name in sorted(filenames):
            full = os.path.join(dirpath, name)
            arc = os.path.join(pkg, os.path.relpath(full, base)).replace(os.sep, "/")
            zf.write(full, arc)
            count += 1
print(f"python zipfile: {count} files")
PY
    ;;
esac

echo "wrote $ARCHIVE"
python - "$ARCHIVE" <<'PY' 2>/dev/null || true
import os, sys, zipfile
p = sys.argv[1]
with zipfile.ZipFile(p) as zf:
    names = zf.namelist()
print(f"entries={len(names)} size={os.path.getsize(p)/1024/1024:.1f}MB")
PY
