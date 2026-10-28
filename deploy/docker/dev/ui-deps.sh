#!/bin/sh
# Installs the UI's npm dependencies into the node_modules volume when ui/package-lock.json
# changed since the last install (development image, compose.yaml).
set -eu
cd "$(dirname "$0")/../../../ui"
want=$(sha256sum package-lock.json | cut -d' ' -f1)
have=$(cat node_modules/.sourcegull-lock-hash 2>/dev/null || true)
if [ "$want" != "$have" ]; then
	echo "ui: installing npm dependencies"
	# node_modules is a volume mount point: empty it rather than letting npm remove it.
	find node_modules -mindepth 1 -maxdepth 1 -exec rm -rf {} +
	npm ci --no-audit --no-fund
	echo "$want" > node_modules/.sourcegull-lock-hash
fi
