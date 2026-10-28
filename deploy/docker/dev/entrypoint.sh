#!/bin/sh
# Entrypoint of the development image: runs the command as `dev` with the UID/GID of the owner
# of /src (the bind-mounted checkout), so build outputs written to the checkout belong to the
# host user. Volumes mounted for caches are handed to that user on first use.
set -eu

if [ "$(id -u)" != 0 ]; then
	exec "$@"
fi

uid=$(stat -c %u /src)
gid=$(stat -c %g /src)
# A root-owned checkout (e.g. some Docker Desktop file sharing modes): stay root.
if [ "$uid" = 0 ]; then
	exec "$@"
fi

if [ "$(id -g dev)" != "$gid" ]; then
	groupmod --non-unique --gid "$gid" dev
fi
if [ "$(id -u dev)" != "$uid" ]; then
	usermod --non-unique --uid "$uid" --gid "$gid" dev
fi

for dir in /cargo /data /src/target /src/ui/node_modules; do
	if [ -d "$dir" ] && [ "$(stat -c %u "$dir")" != "$uid" ]; then
		chown -R "$uid:$gid" "$dir"
	fi
done

export HOME=/home/dev USER=dev
exec setpriv --reuid="$uid" --regid="$gid" --init-groups "$@"
