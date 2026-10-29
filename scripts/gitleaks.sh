#!/usr/bin/env bash
# Secret scan with gitleaks, in Docker.
#
#   scripts/gitleaks.sh staged    the staged changes (the pre-commit hook, scripts/hooks)
#   scripts/gitleaks.sh history   every commit reachable from HEAD (the release workflow)
#
# Rules: .gitleaks.toml, or .git/info/gitleaks.toml when it exists: local additions that must not
# be published (internal host names, addresses), starting with
#   [extend]
#   path = "<absolute path of this checkout>/.gitleaks.toml"
# Install the hook once per clone: git config core.hooksPath scripts/hooks
set -euo pipefail
mode=${1:-staged}
root=$(git rev-parse --show-toplevel)
common=$(cd "$(git rev-parse --git-common-dir)" && pwd)
config=$root/.gitleaks.toml
[ -f "$common/info/gitleaks.toml" ] && config=$common/info/gitleaks.toml

case "$mode" in
  staged) args=(--pre-commit --staged) ;;
  history) args=(--log-opts=HEAD) ;;
  *) echo "usage: scripts/gitleaks.sh staged|history" >&2; exit 2 ;;
esac

# The checkout and the git directory at their own paths, so worktrees resolve too.
exec docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*' \
  -v "$root:$root:ro" -v "$common:$common:ro" -v "$config:$config:ro" -w "$root" \
  zricethezav/gitleaks:v8.28.0 git "${args[@]}" --no-banner --redact --verbose -c "$config" "$root"
