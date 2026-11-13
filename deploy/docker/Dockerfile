# syntax=docker/dockerfile:1.7
#
# Main Sourcegull image (`ghcr.io/sourcegull/sourcegull`).
#
# Build from the repository root:
#   docker buildx build -f deploy/docker/Dockerfile -t ghcr.io/sourcegull/sourcegull:dev .
#
# Stages:
#   toolchain - the Rust toolchain (Debian, glibc), on the build platform
#   sysroot   - the target architecture's Alpine musl-dev and gcc (crt files, libgcc), fetched on
#               the build platform with apk --arch: what the builder's C compiler links against
#   wasm     - the query box parser (crates/query-wasm -> /out/query.wasm)
#   ui       - builds the web UI (ui/ -> /ui/build) with Node.js, with query.wasm in static/
#   builder  - builds static (musl) `sourcegull` and `gull` binaries for the target platform,
#              always as a cross build from the glibc toolchain (so linux/arm64 also builds on
#              amd64 without emulation)
#   cli      - `ghcr.io/sourcegull/gull`: Alpine with git, ca-certificates and `gull` only, for
#              CI jobs (`--target cli`; deploy/gitlab-ci)
#   runtime  - Alpine with git, ca-certificates, tini and bash (final image; also ships `gull`)
#
# Base images: Alpine rather than Debian slim because image scanners report several hundred
# CVEs in Debian's base packages that Debian does not fix in a stable release; the Alpine images
# carry next to none. The binaries are static, so the runtime image only needs the tools above.
# The compiler itself runs on glibc: rustc and LLVM on musl spend much of a release build
# contending for musl's malloc (the final thin-LTO step takes about twice as long).
#
# UI: the `sourcegull-web-assets` crate embeds the built UI into the binary. The `ui` stage
# builds it and the builder points the crate at the result through SOURCEGULL_UI_BUILD_DIR, so a
# stale ui/build in the build context is never embedded.
#
# Caching: target/ is a cache mount kept by the builder, so a rebuild compiles only the crates
# (dependencies and workspace) that changed since the last build on that builder. Its id names the
# Rust version: a new toolchain starts from an empty target/, and the old one ages out of the
# builder's cache instead of piling up next to it (cargo never deletes old outputs). VERSION and
# REVISION are declared only after the expensive steps: a build argument invalidates every RUN
# after its declaration.

ARG RUST_VERSION=1.97
ARG ALPINE_VERSION=3.24
ARG DEBIAN_SUITE=bookworm
ARG NODE_VERSION=22

############################
# toolchain
############################
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS toolchain
# The repository's rust-toolchain.toml asks for "stable"; pin to the image's own toolchain so the
# build never downloads another one (RUST_VERSION is set by the rust base image).
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
WORKDIR /src

############################
# wasm
############################
FROM toolchain AS wasm
ARG RUST_VERSION
RUN rustup target add wasm32-unknown-unknown
COPY . .
RUN --mount=type=cache,id=sourcegull-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=sourcegull-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=sourcegull-wasm-target-${RUST_VERSION},target=/src/target,sharing=locked \
    cargo build --release --locked -p sourcegull-query-wasm --target wasm32-unknown-unknown \
 && install -D -m 0644 target/wasm32-unknown-unknown/release/sourcegull_query_wasm.wasm /out/query.wasm

############################
# ui
############################
FROM --platform=$BUILDPLATFORM node:${NODE_VERSION}-${DEBIAN_SUITE}-slim AS ui
WORKDIR /ui
COPY ui/package.json ui/package-lock.json ./
RUN --mount=type=cache,id=sourcegull-npm,target=/root/.npm,sharing=locked \
    npm ci --no-audit --no-fund
COPY ui/ ./
COPY --from=wasm /out/query.wasm static/query.wasm
RUN rm -rf build && npm run build && test -f build/index.html

############################
# sysroot
############################
FROM --platform=$BUILDPLATFORM alpine:${ALPINE_VERSION} AS sysroot
ARG TARGETARCH
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64 ;; \
      arm64) arch=aarch64 ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    apk add --no-cache --no-scripts --arch "$arch" --root /sysroot --initdb \
      --keys-dir "/usr/share/apk/keys/$arch" --repositories-file /etc/apk/repositories \
      musl-dev gcc

############################
# builder
############################
FROM toolchain AS builder
ARG TARGETARCH
ARG RUST_VERSION
# Optional cap on parallel rustc jobs (e.g. --build-arg CARGO_BUILD_JOBS=3 on a shared machine).
ARG CARGO_BUILD_JOBS
# Optional overrides of the release profile; scripts/image-build.sh sets thin LTO and 16 units,
# which need far less memory than the defaults (fat, 1) at link time.
ARG CARGO_PROFILE_RELEASE_LTO
ARG CARGO_PROFILE_RELEASE_CODEGEN_UNITS
# The Rust (musl) target of TARGETARCH, and its C compiler and linker (target-cc): clang with the
# sysroot stage's files. Build scripts and proc macros are built for the glibc host as usual.
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang lld llvm \
 && rm -rf /var/lib/apt/lists/*
COPY --from=sysroot /sysroot /sysroot
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl; arch=x86_64 ;; \
      arm64) target=aarch64-unknown-linux-musl; arch=aarch64 ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$target" > /rust-target; \
    rustup target add "$target"; \
    printf '#!/bin/sh\nexec clang --target=%s-alpine-linux-musl --sysroot=/sysroot -fuse-ld=lld "$@"\n' \
      "$arch" > /usr/local/bin/target-cc; \
    chmod 0755 /usr/local/bin/target-cc
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=target-cc \
    CC_x86_64_unknown_linux_musl=target-cc \
    AR_x86_64_unknown_linux_musl=llvm-ar \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=target-cc \
    CC_aarch64_unknown_linux_musl=target-cc \
    AR_aarch64_unknown_linux_musl=llvm-ar
COPY . .
COPY --from=ui /ui/build /ui-build
ENV SOURCEGULL_UI_BUILD_DIR=/ui-build
# Both platforms share the cache (their outputs are under target/<triple>/, the build scripts
# and proc macros they share under target/release/).
RUN --mount=type=cache,id=sourcegull-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=sourcegull-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=sourcegull-musl-target-${RUST_VERSION},target=/src/target,sharing=locked \
    target=$(cat /rust-target) \
 && cargo build --release --locked --target "$target" -p sourcegull --bin sourcegull -p sourcegull-cli --bin gull \
 && install -D -m 0755 "target/$target/release/sourcegull" /out/sourcegull \
 && install -D -m 0755 "target/$target/release/gull" /out/gull

############################
# cli
############################
FROM alpine:${ALPINE_VERSION} AS cli
# git lets `gull code-intel upload` infer the repository and commit outside GitLab CI.
RUN apk upgrade --no-cache && apk add --no-cache ca-certificates git
ARG VERSION=0.10.0
LABEL org.opencontainers.image.title="sourcegull-cli" \
      org.opencontainers.image.description="The gull command-line client" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source="https://github.com/sourcegull/sourcegull"
COPY --from=builder /out/gull /usr/local/bin/gull
CMD ["gull", "--help"]

############################
# runtime
############################
FROM alpine:${ALPINE_VERSION} AS runtime

# git is needed by the repo role; git-lfs is not installed. bash runs the Compose healthchecks
# (/dev/tcp). tini is also at /usr/bin/tini, the documented entrypoint path. `apk upgrade` takes
# the fixes published since the base image was built.
RUN apk upgrade --no-cache \
 && apk add --no-cache bash ca-certificates git tini \
 && ln -s /sbin/tini /usr/bin/tini

# Non-root user (UID/GID 10001). The root filesystem can be mounted read-only: everything the
# process writes goes to /var/opt/sourcegull (volume) or /tmp (tmpfs/emptyDir in Compose/Helm).
RUN addgroup -S -g 10001 sourcegull \
 && adduser -S -D -H -u 10001 -G sourcegull -h /var/opt/sourcegull -s /sbin/nologin sourcegull \
 && install -d -o 10001 -g 10001 -m 0750 /var/opt/sourcegull \
 && install -d -o 10001 -g 10001 -m 0755 /etc/sourcegull

COPY --from=builder /out/sourcegull /usr/local/bin/sourcegull
COPY --from=builder /out/gull /usr/local/bin/gull

ARG VERSION=0.10.0
ARG REVISION=unknown
LABEL org.opencontainers.image.title="sourcegull" \
      org.opencontainers.image.description="Self-hosted code intelligence platform" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.source="https://github.com/sourcegull/sourcegull"

ENV SOURCEGULL_DATA_DIR=/var/opt/sourcegull \
    SOURCEGULL_HTTP_ADDR=0.0.0.0:7080 \
    SOURCEGULL_INTERNAL_ADDR=0.0.0.0:6060 \
    SOURCEGULL_REPO_GRPC_ADDR=0.0.0.0:3178 \
    SOURCEGULL_LOG_FORMAT=json \
    HOME=/var/opt/sourcegull

USER 10001:10001
WORKDIR /var/opt/sourcegull
VOLUME ["/var/opt/sourcegull"]
# 7080: public (UI, /.api/*, /healthz, /readyz); 6060: internal (/metrics, /healthz, /readyz);
# 3178: repo node gRPC (`repo` role; unauthenticated, keep it on the internal network).
EXPOSE 7080 6060 3178

# No HEALTHCHECK here: the image ships neither curl nor wget. Compose uses a bash /dev/tcp probe
# and Kubernetes uses httpGet probes on /healthz and /readyz.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/sourcegull"]
CMD ["run", "--roles=all"]
