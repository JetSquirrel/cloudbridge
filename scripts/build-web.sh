#!/usr/bin/env bash
#
# Build the web demo: the same crate as the desktop application, compiled for
# wasm32 and wrapped in the small static site under `web/site/`.
#
# Two things here are not obvious.
#
# Nightly: `gpui-pre-web` pulls Zed's `wasm_thread`, which uses a
# `stdarch_wasm_atomic_wait` feature that is not on stable. The desktop build
# is unaffected — it stays on stable. The nightly is pinned to a date rather
# than floating: an unstable feature can be renamed or removed overnight, and
# a floating `+nightly` turns that into a build that breaks with nobody having
# changed anything. Bump WEB_TOOLCHAIN deliberately, or set it in the
# environment to try another one.
#
# The icons: gpui-kit's wasm asset source fetches SVG icons from
# `<endpoint>/assets/icons/<name>.svg` on demand rather than embedding them,
# so the directory has to be served. It is copied out of the exact crate
# version `Cargo.lock` resolved — a glob over the registry would silently pick
# whichever version sorted first when more than one is unpacked.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

WEB_TOOLCHAIN="${WEB_TOOLCHAIN:-nightly-2026-07-28}"

if ! rustup run "$WEB_TOOLCHAIN" rustc --version >/dev/null 2>&1; then
    echo "error: the web build needs the $WEB_TOOLCHAIN toolchain. Install it with:" >&2
    echo "    rustup toolchain install $WEB_TOOLCHAIN --target wasm32-unknown-unknown" >&2
    echo "or build with the nightly you already have:" >&2
    echo "    WEB_TOOLCHAIN=nightly $0 ${1:-}" >&2
    exit 1
fi

echo "==> Building the wasm module with $WEB_TOOLCHAIN"
if [[ "${1:-}" == "--release" ]]; then
    profile="release"
    cargo "+$WEB_TOOLCHAIN" build --lib --release --target wasm32-unknown-unknown
else
    profile="debug"
    cargo "+$WEB_TOOLCHAIN" build --lib --target wasm32-unknown-unknown
fi

wasm="target/wasm32-unknown-unknown/$profile/cloudbridge.wasm"

if [[ ! -f "$wasm" ]]; then
    echo "error: no wasm module at $wasm" >&2
    exit 1
fi

echo "==> Generating the JavaScript bindings"
# A CLI older or newer than the crate emits bindings the module does not
# match, and the failure surfaces in the browser rather than here.
bindgen_version="$(scripts/locked-version.py wasm-bindgen)"
installed_version="$(wasm-bindgen --version 2>/dev/null | awk '{print $2}')"
if [[ "$installed_version" != "$bindgen_version" ]]; then
    echo "error: wasm-bindgen CLI is ${installed_version:-missing}, but Cargo.lock names $bindgen_version." >&2
    echo "    cargo install wasm-bindgen-cli --version $bindgen_version --locked" >&2
    exit 1
fi

mkdir -p web/site/src/wasm
wasm-bindgen "$wasm" \
    --out-dir web/site/src/wasm \
    --target web \
    --no-typescript

echo "==> Collecting the icon set"
assets_version="$(scripts/locked-version.py gpui-kit-assets)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
assets_dirs=()
while IFS= read -r dir; do
    assets_dirs+=("$dir")
done < <(
    find "$cargo_home/registry/src" -maxdepth 2 -type d \
        -name "gpui-kit-assets-$assets_version" 2>/dev/null
)
if [[ ${#assets_dirs[@]} -ne 1 ]]; then
    echo "error: expected one unpacked gpui-kit-assets-$assets_version under" >&2
    echo "       $cargo_home/registry/src, found ${#assets_dirs[@]}" >&2
    exit 1
fi
assets_dir="${assets_dirs[0]}/assets"
rm -rf web/site/assets
mkdir -p web/site/assets
cp -R "$assets_dir/icons" web/site/assets/icons

echo
echo "Built. To run it:"
echo "    python3 -m http.server 8000 --directory web/site"
echo "then open http://localhost:8000/"
