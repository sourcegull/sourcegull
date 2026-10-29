#!/usr/bin/env bash
# Secret scan with gitleaks, in Docker.
#
#   scripts/gitleaks.sh staged    the staged changes (the pre-commit hook, scripts/hooks)
#   scripts/gitleaks.sh history   every commit reachable from HEAD (the release workflow)
#
# Rules: this checkout's .gitleaks.toml, plus the [[rules]] in .git/info/gitleaks.toml when it
# exists (shared by all worktrees): local additions that must not be published, such as internal
# host names and addresses.
# Install the hook once per clone: git config core.hooksPath scripts/hooks
set -euo pipefail
mode=${1:-staged}
root=$(git rev-parse --show-toplevel)
common=$(cd "$(git rev-parse --git-common-dir)" && pwd)
config=$root/.gitleaks.toml
if [ -f "$common/info/gitleaks.toml" ]; then
  merged=$(mktemp "${TMPDIR:-/tmp}/gitleaks.XXXXXX.toml")
  trap 'rm -f "$merged"' EXIT
  cat "$root/.gitleaks.toml" "$common/info/gitleaks.toml" > "$merged"
  chmod 0644 "$merged"
  config=$merged
fi

case "$mode" in
  staged) args=(--pre-commit --staged) ;;
  history) args=(--log-opts=HEAD) ;;
  *) echo "usage: scripts/gitleaks.sh staged|history" >&2; exit 2 ;;
esac

# The checkout and the git directory at their own paths, so worktrees resolve too.
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*' \
  -v "$root:$root:ro" -v "$common:$common:ro" -v "$config:$config:ro" -w "$root" \
  zricethezav/gitleaks:v8.28.0 git "${args[@]}" --no-banner --redact --verbose -c "$config" "$root"
