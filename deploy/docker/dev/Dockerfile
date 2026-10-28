# syntax=docker/dockerfile:1.7
#
# Development image (`sourcegull-dev`) used by the root compose.yaml: the Rust toolchain (with the
# wasm32 target for the query box parser), Node.js for the UI, the Postgres client and git.
# The checkout is bind-mounted at /src; nothing from the repository is copied in, so the image
# only rebuilds when this file changes.
#
# Stages:
#   dev  - everything needed to build, test and run Sourcegull and the UI
#   e2e  - dev plus Playwright with Chromium, for scripts/test/e2e-ui.sh

ARG RUST_VERSION=1.97
ARG DEBIAN_SUITE=bookworm
ARG NODE_VERSION=22

FROM node:${NODE_VERSION}-${DEBIAN_SUITE}-slim AS node

############################
# dev
############################
FROM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS dev

# The repository's rust-toolchain.toml asks for "stable"; pin to the image's own toolchain so
# cargo never downloads another one (RUST_VERSION is set by the rust base image).
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
RUN rustup component add rustfmt clippy rust-src \
 && rustup target add wasm32-unknown-unknown

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && node --version && npm --version

RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-client jq less \
 && rm -rf /var/lib/apt/lists/*

# The entrypoint gives `dev` the UID/GID of the checkout's owner, so files written to /src belong
# to the host user. CARGO_HOME (registry, git checkouts, `cargo install`ed tools) is a volume;
# the toolchain itself stays in the image (/usr/local/cargo/bin, /usr/local/rustup).
RUN groupadd --gid 1000 dev \
 && useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash dev \
 && install -d -o dev -g dev /cargo /data \
 && git config --system safe.directory '*'
ENV CARGO_HOME=/cargo \
    PATH=/usr/local/cargo/bin:/cargo/bin:${PATH} \
    CARGO_TERM_COLOR=always

COPY --chmod=0755 entrypoint.sh /usr/local/bin/dev-entrypoint
WORKDIR /src
ENTRYPOINT ["/usr/local/bin/dev-entrypoint"]
CMD ["bash"]

############################
# e2e
############################
FROM dev AS e2e
ARG PLAYWRIGHT_VERSION=1.49.1
ENV PLAYWRIGHT_BROWSERS_PATH=/opt/playwright/browsers \
    NODE_PATH=/opt/playwright/node_modules
RUN mkdir -p /opt/playwright \
 && cd /opt/playwright \
 && npm init -y >/dev/null \
 && npm install --no-audit --no-fund "playwright@${PLAYWRIGHT_VERSION}" \
 && npx playwright install --with-deps chromium \
 && rm -rf /var/lib/apt/lists/* /root/.npm \
 && chmod -R a+rX /opt/playwright
