# Local real-btrfs harness for the decmpfs crate. Optimized to the bone:
#   * tiny build context (Dockerfile.dockerignore drops target/ etc.),
#   * BuildKit cache mounts for the cargo registry + target (incremental rebuilds),
#   * the btrfs test is built as a fully-static, upx-packed musl binary, and
#   * the runtime is FROM scratch: just static busybox (shell + mount/modprobe),
#     mkfs.btrfs with its ldd'd libs, and the test binary — no OS rootfs.
# Build (context = the crate dir):
#   docker build -f ci/Dockerfile -t decmpfs-btrfs .
# Run (needs --privileged to loop-mount btrfs):
#   docker run --rm --privileged decmpfs-btrfs
# syntax=docker/dockerfile:1
FROM rust:1-alpine AS builder
# binutils(strip) + upx pack the binary; btrfs-progs + busybox-static are harvested
# into the scratch rootfs below. No gcc/musl-dev: the crate's only dep is libc
# (pure FFI, no C build), so the alpine/musl `cargo test` needs no C toolchain.
RUN apk add --no-cache binutils upx btrfs-progs busybox-static
WORKDIR /crate

COPY Cargo.toml ./
COPY src ./src
COPY tests ./tests
COPY ci/btrfs-loopback.sh ci/run.sh /src-ci/

# Only the btrfs integration test belongs here (unit tests run on 3 OSes in CI).
# Collect just this build's binary via cargo's JSON message stream.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/crate/target \
    mkdir /bins \
    && cargo test --no-run --test btrfs --message-format=json \
       | grep -o '"executable":"[^"]*"' | cut -d'"' -f4 \
       | while read -r bin; do cp "$bin" /bins/; done \
    && strip /bins/* && upx --best /bins/*

# Assemble a minimal rootfs for scratch: static busybox + symlinks for the applets
# the scripts call (the rest are sh builtins), mkfs.btrfs with its shared libs, the
# binary, and the scripts.
RUN set -eu; R=/rootfs; \
    mkdir -p "$R/bin" "$R/sbin" "$R/ci" "$R/bins" "$R/tmp" "$R/mnt" "$R/proc" "$R/sys" "$R/dev"; \
    cp /bin/busybox.static "$R/bin/busybox"; \
    for applet in sh grep id mkdir chmod head tee sync mount umount modprobe basename truncate; do \
      ln -s busybox "$R/bin/$applet"; \
    done; \
    mkfs=$(command -v mkfs.btrfs); cp "$mkfs" "$R/sbin/mkfs.btrfs"; \
    for lib in $(ldd "$mkfs" | grep -oE '/[^ ]+\.so[^ ]*' | sort -u); do \
      mkdir -p "$R$(dirname "$lib")"; cp "$lib" "$R$lib"; \
    done; \
    cp /src-ci/btrfs-loopback.sh /src-ci/run.sh "$R/ci/"; chmod +x "$R/ci/run.sh"; \
    cp /bins/* "$R/bins/"

FROM scratch
COPY --from=builder /rootfs /
ENV PATH=/bin:/sbin:/usr/sbin
CMD ["/bin/sh", "/ci/run.sh"]
