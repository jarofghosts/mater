#!/usr/bin/env bash
#
# Cross-compile the Schwung module for the Move and package it.
#
# Re-runs itself inside Docker unless it is already in a container, so the host needs nothing but
# Docker. Output:
#
#   dist/mater/{module.json,dsp.so}
#   dist/mater-module.tar.gz          <- what a release attaches
set -euo pipefail

MODULE_ID=mater
TARGET=aarch64-unknown-linux-gnu
IMAGE=mater-schwung-build

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

if [ ! -f /.dockerenv ]; then
    echo "==> building $IMAGE"
    docker build -t "$IMAGE" "$ROOT/schwung"

    echo "==> building $MODULE_ID for $TARGET"
    # Run as the invoking user so nothing under target/ or dist/ comes back root-owned. CARGO_HOME
    # goes into the tree for the same reason, and doubles as a registry cache between runs.
    exec docker run --rm \
        --user "$(id -u):$(id -g)" \
        -v "$ROOT:/work" \
        -w /work \
        -e CARGO_HOME=/work/target/schwung-docker/cargo \
        -e CARGO_TARGET_DIR=/work/target/schwung-docker \
        "$IMAGE" \
        schwung/build.sh "$@"
fi

cd "$ROOT"

# `-p` keeps this to granny-core plus the wrapper. Building the whole workspace would drag in
# nih-plug, egui and symphonia, none of which this module uses and none of which need to
# cross-compile.
cargo build --release -p mater-schwung --target "$TARGET"

BUILT="${CARGO_TARGET_DIR:-$ROOT/target}/$TARGET/release/libmater_schwung.so"
[ -f "$BUILT" ] || { echo "error: $BUILT not found" >&2; exit 1; }

STAGE="$ROOT/dist/$MODULE_ID"
rm -rf "$STAGE"
mkdir -p "$STAGE"

# The chain host loads a sound generator's shared library as `dsp.so`, by that name.
cp "$BUILT" "$STAGE/dsp.so"
cp "$ROOT/schwung/module.json" "$STAGE/module.json"
aarch64-linux-gnu-strip "$STAGE/dsp.so" 2>/dev/null || true

tar -czf "$ROOT/dist/$MODULE_ID-module.tar.gz" -C "$ROOT/dist" "$MODULE_ID"

echo
echo "==> dist/$MODULE_ID-module.tar.gz"
ls -lh "$ROOT/dist/$MODULE_ID-module.tar.gz" "$STAGE/dsp.so" | sed 's/^/    /'
file "$STAGE/dsp.so" 2>/dev/null | sed 's/^/    /' || true
