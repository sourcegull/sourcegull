# syntax=docker/dockerfile:1.7
#
# Main Sourcegull image (`ghcr.io/sourcegull/sourcegull`).
#
# Build from the repository root:
#   docker buildx build -f deploy/docker/Dockerfile -t ghcr.io/sourcegull/sourcegull:dev .
#
# Stages:
#   toolchain - the Rust toolchain, on the build platform
#   wasm     - the query box parser (crates/query-wasm -> /out/query.wasm)
#   ui       - builds the web UI (ui/ -> /ui/build) with Node.js, with query.wasm in static/
#   builder  - builds the `sourcegull` and `gull` binaries for the target platform
#              (cross-compiled when it differs from the build platform, so linux/arm64 builds on
#              amd64 without emulation)
#   cli      - `ghcr.io/sourcegull/gull`: debian:bookworm-slim with git, ca-certificates and `gull` only, for
#              CI jobs (`--target cli`; deploy/gitlab-ci)
#   runtime  - debian:bookworm-slim with git, ca-certificates and tini (final image; also ships
#              `gull`)
#
# UI: the `sourcegull-web-assets` crate embeds the built UI into the binary. The `ui` stage
# builds it and the builder points the crate at the result through SOURCEGULL_UI_BUILD_DIR, so a
# stale ui/build in the build context is never embedded.
#
# Caching: target/ is a cache mount kept by the builder, so a rebuild compiles only the crates
# (dependencies and workspace) that changed since the last build on that builder. VERSION and
# REVISION are declared only after the expensive steps: a build argument invalidates every RUN
# after its declaration.

ARG RUST_VERSION=1.97
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
RUN rustup target add wasm32-unknown-unknown
COPY . .
RUN --mount=type=cache,id=sourcegull-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=sourcegull-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=sourcegull-wasm-target,target=/src/target,sharing=locked \
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
# builder
############################
FROM toolchain AS builder
ARG TARGETARCH
# Optional cap on parallel rustc jobs (e.g. --build-arg CARGO_BUILD_JOBS=3 on a shared machine).
ARG CARGO_BUILD_JOBS
# Optional overrides of the release profile; scripts/image-build.sh sets thin LTO and 16 units,
# which need far less memory than the defaults (fat, 1) at link time.
ARG CARGO_PROFILE_RELEASE_LTO
ARG CARGO_PROFILE_RELEASE_CODEGEN_UNITS
# The Rust target of TARGETARCH, and a cross toolchain when it is not the build platform's.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu; gcc=gcc-x86-64-linux-gnu; gxx=g++-x86-64-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu; gcc=gcc-aarch64-linux-gnu; gxx=g++-aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$target" > /rust-target; \
    if [ "$(dpkg --print-architecture)" != "$TARGETARCH" ]; then \
      apt-get update \
      && apt-get install -y --no-install-recommends "$gcc" "$gxx" "libc6-dev-$TARGETARCH-cross" \
      && rm -rf /var/lib/apt/lists/* \
      && rustup target add "$target"; \
    fi
# Debian names its compilers by GNU triple, natively too, so these hold for both directions.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
COPY . .
COPY --from=ui /ui/build /ui-build
ENV SOURCEGULL_UI_BUILD_DIR=/ui-build
# Both platforms share the cache (their outputs are under target/<triple>/, the build scripts
# and proc macros they share under target/release/).
RUN --mount=type=cache,id=sourcegull-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=sourcegull-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=sourcegull-target,target=/src/target,sharing=locked \
    target=$(cat /rust-target) \
 && cargo build --release --locked --target "$target" -p sourcegull --bin sourcegull -p sourcegull-cli --bin gull \
 && install -D -m 0755 "target/$target/release/sourcegull" /out/sourcegull \
 && install -D -m 0755 "target/$target/release/gull" /out/gull

############################
# cli
############################
FROM debian:${DEBIAN_SUITE}-slim AS cli
# git lets `gull code-intel upload` infer the repository and commit outside GitLab CI.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* /var/log/apt /var/log/dpkg.log
ARG VERSION=0.1.0
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
FROM debian:${DEBIAN_SUITE}-slim AS runtime

# git is needed by the repo role; git-lfs is not installed.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git tini \
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* /var/log/apt /var/log/dpkg.log

# Non-root user (UID/GID 10001). The root filesystem can be mounted read-only: everything the
# process writes goes to /var/opt/sourcegull (volume) or /tmp (tmpfs/emptyDir in Compose/Helm).
RUN groupadd --system --gid 10001 sourcegull \
 && useradd --system --uid 10001 --gid 10001 --home-dir /var/opt/sourcegull --no-create-home \
      --shell /usr/sbin/nologin sourcegull \
 && install -d -o 10001 -g 10001 -m 0750 /var/opt/sourcegull \
 && install -d -o 10001 -g 10001 -m 0755 /etc/sourcegull

COPY --from=builder /out/sourcegull /usr/local/bin/sourcegull
COPY --from=builder /out/gull /usr/local/bin/gull

ARG VERSION=0.1.0
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
