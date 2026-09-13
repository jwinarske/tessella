#!/usr/bin/env bash
# Builds render_probe and compiles the materials it binds, from the current trees.
#
# Both are build outputs and both land in PARITY_WORK. The probe links the producer's static
# library, so this rebuilds tessella too -- which is the point: a parity run measures the tree as
# it stands, not the last binary anybody happened to build.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

cd "$TESSELLA_DIR" && cargo build -p tessella-ffi --features tls >/dev/null

mkdir -p "$PARITY_WORK/mat"
n=0
for m in "$TESSELLA_FLUORITE"/materials/*.mat; do
  # -p all, not -p desktop: Filament resolves Vulkan on a mobile GPU as the mobile shader model
  # and refuses a desktop-only package, which renders black rather than failing.
  "$FILAMENT_STAGING/bin/matc" -a vulkan -p all \
    -o "$PARITY_WORK/mat/$(basename "${m%.mat}").filamat" "$m"
  n=$((n + 1))
done

ninja -C "$PARITY_WORK/consumer" tsf_consumer >/dev/null 2>&1 || {
  cmake -S "$TESSELLA_FLUORITE" -B "$PARITY_WORK/consumer" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DTESSELLA_DIR="$TESSELLA_DIR" \
    -DTESSELLA_LIB="$TESSELLA_DIR/target/debug/libtessella_ffi.a" \
    -DFILAMENT_INCLUDE_DIR="$FILAMENT_STAGING/include" >/dev/null
  ninja -C "$PARITY_WORK/consumer" tsf_consumer >/dev/null
}

# Both probes, same link line. `quad_probe` is the other half of the smoke test -- four views
# on one engine over `quad_remote.json`, which is what catches a regression in layer masking or
# in the shared scene that a single-view sweep cannot see.
probe() {
  clang++ -std=c++20 -stdlib=libc++ -O1 -g \
    -I"$TESSELLA_FLUORITE/native/include" -I"$TESSELLA_DIR/include" -I"$FILAMENT_STAGING/include" \
    "$TESSELLA_FLUORITE/native/test/$1.cc" \
    "$PARITY_WORK/consumer/libtsf_consumer.a" \
    -Wl,--start-group \
    "$FILAMENT_STAGING"/lib/x86_64/lib{filament,backend,bluevk,bluegl,filabridge,filaflat,utils,geometry,smol-v,vkshaders,ibl,zstd}.a \
    -Wl,--end-group \
    "$TESSELLA_DIR/target/debug/libtessella_ffi.a" \
    -lpthread -ldl -lm -lEGL -lGL \
    -o "$PARITY_WORK/$1"
}

probe render_probe
probe quad_probe

echo "built $PARITY_WORK/{render_probe,quad_probe} and $n material packages"
