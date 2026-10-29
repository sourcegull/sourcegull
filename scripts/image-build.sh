#!/usr/bin/env bash
# `docker buildx build` of deploy/docker/Dockerfile on this machine, with limits so a build cannot
# take the host down: a dedicated builder (container `buildx_buildkit_sourcegull-limited0`) capped
# at SOURCEGULL_BUILD_CPUS cores and SOURCEGULL_BUILD_MEMORY of memory with no swap (a build that
# needs more fails with an out-of-memory error instead of paging), CARGO_BUILD_JOBS rustc jobs,
# and thin LTO (fat LTO's single link step needs far more memory), as in the release workflow.
#
# Usage: scripts/image-build.sh [--test] [docker buildx build args...] <context>
#   e.g. scripts/image-build.sh -f deploy/docker/Dockerfile --load -t sourcegull/sourcegull:dev .
#   --test  an image for trying out a change (e.g. on a local cluster): no LTO and 256 codegen
#           units, so a small change rebuilds in a fraction of the time. The binary is a little
#           slower and larger: never measure performance with it.
#   SOURCEGULL_BUILD_CPUS    cores (default 4)
#   SOURCEGULL_BUILD_MEMORY  memory limit (default 8g)
#   CARGO_BUILD_JOBS         parallel rustc jobs (default 3)
#   SOURCEGULL_BUILD_LTO     the release profile's LTO (default thin; `fat` as in releases)
#   SOURCEGULL_BUILD_CODEGEN_UNITS  its codegen units (default 16; 1 in releases)
# After changing the limits: docker buildx rm sourcegull-limited
set -euo pipefail
if [ "${1:-}" = --test ]; then
  shift
  # The UI is embedded in the binary, so any change relinks it; with LTO that link dominates.
  : "${SOURCEGULL_BUILD_LTO:=off}" "${SOURCEGULL_BUILD_CODEGEN_UNITS:=256}"
fi
BUILDER=sourcegull-limited
CPUS=${SOURCEGULL_BUILD_CPUS:-4}
MEMORY=${SOURCEGULL_BUILD_MEMORY:-8g}

if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
  docker buildx create --name "$BUILDER" --driver docker-container \
    --driver-opt "cpuset-cpus=0-$((CPUS - 1)),memory=$MEMORY,memory-swap=$MEMORY" >/dev/null
fi

exec docker buildx build --builder "$BUILDER" \
  --build-arg CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}" \
  --build-arg CARGO_PROFILE_RELEASE_LTO="${SOURCEGULL_BUILD_LTO:-thin}" \
  --build-arg CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${SOURCEGULL_BUILD_CODEGEN_UNITS:-16}" \
  "$@"
