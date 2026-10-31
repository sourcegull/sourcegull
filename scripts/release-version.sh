#!/usr/bin/env bash
# Sets the release version in every file that carries it, as the release workflow does before it
# builds:
#
# - the workspace version (Cargo.toml, the workspace crates in Cargo.lock), the UI package,
#   `ARG VERSION` in the Dockerfile, `SOURCEGULL_VERSION` in the Compose files, `.env.example` and
#   the upgrade guide, and the default image version named in docs/install.md;
# - the chart's `appVersion`, and its `version` with a patch bump unless the chart version has
#   changed since the previous release already (a row for it goes into the chart README's table);
# - for a release that is not a pre-release: `crates/db/schema/<version>.json` (a copy of
#   current.json) and `release!("<version>", <newest migration>)` in `RELEASES`
#   (crates/db/src/upgrade.rs), for `migrate upgrade`.
#
# Usage: scripts/release-version.sh <patch|minor|major|X.Y.Z|X.Y.Z-pre>
#   patch, minor and major bump the current version; from a pre-release (0.3.0-rc.1) each of
#   them gives its release (0.3.0). Prints the new version on stdout.
set -euo pipefail
cd "$(dirname "$0")/.."

die() { echo "release-version: $*" >&2; exit 1; }
semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

arg=${1:?usage: scripts/release-version.sh <patch|minor|major|X.Y.Z[-pre]>}
old=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[[ $old =~ $semver ]] || die "no workspace version in Cargo.toml"

base=${old%%-*}
IFS=. read -r major minor patch <<<"$base"
case "$arg" in
  patch | minor | major)
    if [ "$old" != "$base" ]; then
      new=$base
    else
      case "$arg" in
        patch) new=$major.$minor.$((patch + 1)) ;;
        minor) new=$major.$((minor + 1)).0 ;;
        major) new=$((major + 1)).0.0 ;;
      esac
    fi
    ;;
  *)
    new=${arg#v}
    [[ $new =~ $semver ]] || die "not a version or patch|minor|major: $arg"
    ;;
esac
[ "$new" != "$old" ] || die "the version is $old already"
# sort -V puts 0.3.0-rc.1 after 0.3.0, so compare the release parts and order pre-releases
# before their release by hand.
newer() {
  local a=$1 b=$2
  [ "${a%%-*}" != "${b%%-*}" ] && { [ "$(printf '%s\n%s\n' "${a%%-*}" "${b%%-*}" | sort -V | tail -n 1)" = "${a%%-*}" ]; return; }
  [ "$a" = "${a%%-*}" ] && return 0
  [ "$b" = "${b%%-*}" ] && return 1
  [ "$(printf '%s\n%s\n' "$a" "$b" | sort -V | tail -n 1)" = "$a" ]
}
newer "$new" "$old" || die "$new is not newer than $old"

# Replaces exactly what is expected: fails when a file does not contain the old text.
replace() { # file sed-expression grep-pattern-of-the-new-text
  local file=$1 expr=$2 check=$3
  sed -i -E "$expr" "$file"
  grep -qE "$check" "$file" || die "$file: version not found ($check)"
}
o=${old//./\\.}

replace Cargo.toml "0,/^version = \"$o\"\$/s//version = \"$new\"/" "^version = \"$new\"\$"

# Workspace crates in Cargo.lock: the packages named in crates/*/Cargo.toml, at the old version.
names=$(sed -n 's/^name = "\(.*\)"$/\1/p' crates/*/Cargo.toml | sort -u | tr '\n' ' ')
awk -v names=" $names" -v old="$old" -v new="$new" '
  BEGIN { n = 0 }
  /^name = "/ { name = $3; gsub(/"/, "", name); pending = index(names, " " name " ") > 0; print; next }
  pending && $0 == "version = \"" old "\"" { print "version = \"" new "\""; pending = 0; n++; next }
  { pending = 0; print }
  END { if (n == 0) exit 1 }
' Cargo.lock >Cargo.lock.new || die "Cargo.lock: no workspace crate at $old"
mv Cargo.lock.new Cargo.lock

replace ui/package.json "0,/\"version\": \"$o\"/s//\"version\": \"$new\"/" "\"version\": \"$new\""
# The root package's two entries, at the top of the lockfile.
sed -i -E "1,12s/^(  |      )\"version\": \"$o\"/\1\"version\": \"$new\"/" ui/package-lock.json
[ "$(head -n 12 ui/package-lock.json | grep -c "\"version\": \"$new\"")" = 2 ] ||
  die "ui/package-lock.json: root version not found"

replace deploy/docker/Dockerfile "s/^ARG VERSION=$o\$/ARG VERSION=$new/" "^ARG VERSION=$new\$"
for f in deploy/docker/compose*.yaml; do
  if grep -q 'SOURCEGULL_VERSION:-' "$f"; then
    replace "$f" "s/SOURCEGULL_VERSION:-$o\}/SOURCEGULL_VERSION:-$new}/g" "SOURCEGULL_VERSION:-$new\}"
  fi
done
replace deploy/docker/.env.example "s/^SOURCEGULL_VERSION=$o\$/SOURCEGULL_VERSION=$new/" "^SOURCEGULL_VERSION=$new\$"
replace deploy/docker/upgrade.md \
  "s#/download/v$o/#/download/v$new/#; s#SOURCEGULL_VERSION=$o/#SOURCEGULL_VERSION=$new/#" \
  "/download/v$new/"
replace docs/install.md "s/appVersion\` \(\`$o\`\)/appVersion\` (\`$new\`)/" "appVersion\` \(\`$new\`\)"

# Chart: appVersion, and a patch bump of the chart version unless it changed since the previous
# release (the newest v* tag reachable from HEAD).
chart=deploy/helm/sourcegull/Chart.yaml
replace "$chart" "s/^appVersion: .*/appVersion: \"$new\"/" "^appVersion: \"$new\"\$"
chart_version=$(sed -n 's/^version: //p' "$chart")
previous_tag=$(git describe --tags --abbrev=0 --match 'v[0-9]*' HEAD 2>/dev/null || true)
released_chart=
[ -n "$previous_tag" ] && released_chart=$(git show "$previous_tag:$chart" 2>/dev/null | sed -n 's/^version: //p')
if [ -z "$released_chart" ] || [ "$chart_version" = "$released_chart" ]; then
  IFS=. read -r cmajor cminor cpatch <<<"${chart_version%%-*}"
  chart_version=$cmajor.$cminor.$((cpatch + 1))
  replace "$chart" "s/^version: .*/version: $chart_version/" "^version: $chart_version\$"
fi
readme=deploy/helm/sourcegull/README.md
if ! grep -q "^| $chart_version |" "$readme"; then
  # After the last row of the table under "| Chart | Changes |".
  awk -v row="| $chart_version | Sourcegull $new |" '
    /^\| Chart \| Changes \|/ { table = 1 }
    table && !/^\|/ { print row; table = 0; done = 1 }
    { print }
    END { if (!done) exit 1 }
  ' "$readme" >"$readme.new" || die "$readme: no chart version table"
  mv "$readme.new" "$readme"
fi

# Schema description and RELEASES entry (not for pre-releases).
if [ "$new" = "${new%%-*}" ]; then
  newest=$(ls crates/db/migrations/main/*.up.sql | sed 's#.*/\([0-9]*\)_.*#\1#' | sort -n | tail -n 1)
  [ -n "$newest" ] || die "no migrations in crates/db/migrations/main"
  cp crates/db/schema/current.json "crates/db/schema/$new.json"
  awk -v line="    release!(\"$new\", $newest)," '
    /^pub const RELEASES: / { in_list = 1 }
    in_list && /^\];/ { print line; in_list = 0; done = 1 }
    { print }
    END { if (!done) exit 1 }
  ' crates/db/src/upgrade.rs >crates/db/src/upgrade.rs.new || die "upgrade.rs: RELEASES not found"
  mv crates/db/src/upgrade.rs.new crates/db/src/upgrade.rs
fi

echo "release-version: $old -> $new (chart $chart_version)" >&2
echo "$new"
