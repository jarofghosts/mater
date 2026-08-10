# Cross-compiler for the Move's ARM.
#
# Debian bookworm to match what Schwung's own `scripts/build.sh` builds in: the module links
# against the device's glibc, so the two have to agree.
FROM rust:1-bookworm

# `libc6-dev-arm64-cross` is only a *recommend* of the cross-gcc, so `--no-install-recommends`
# leaves the linker without crti.o and the build fails at the very last step. Named explicitly.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        gcc-aarch64-linux-gnu \
        libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add aarch64-unknown-linux-gnu

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
