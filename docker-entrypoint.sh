#!/bin/sh
# Run as whoever owns the folder mounted at /srv, so what OpenSlackTag
# writes there (.env, data/) belongs to that user on the host, whatever
# their uid. With the Docker socket mounted (the sandbox override), also join
# the socket's group so it can start sandbox containers.
set -e

uid=$(stat -c %u /srv)
gid=$(stat -c %g /srv)
if [ "$(id -u)" != 0 ] || [ "$uid" = 0 ]; then
    exec openslacktag "$@"
fi

groups=$gid
if [ -S /var/run/docker.sock ]; then
    groups="$groups,$(stat -c %g /var/run/docker.sock)"
fi
as_owner="setpriv --reuid=$uid --regid=$gid --groups=$groups"

# Some Docker setups only let root use the socket; then stay root.
if [ -S /var/run/docker.sock ] && ! $as_owner test -w /var/run/docker.sock; then
    echo "Running as root: the Docker socket is not usable by uid $uid." >&2
    exec openslacktag "$@"
fi

export HOME=/tmp/openslacktag-home
mkdir -p "$HOME"
chown "$uid:$gid" "$HOME"
exec $as_owner openslacktag "$@"
