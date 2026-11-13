#!/usr/bin/env bash
# Package the release binaries for one target into dist/:
#   imessage-backup-<tag>-<target>.tar.gz  (+ .sha256)
# Usage: package.sh <tag> <target> <dir containing imsg-extract and imsg-server>
set -euo pipefail
tag="$1" target="$2" bindir="$3"
name="imessage-backup-${tag}-${target}"
stage="dist/${name}"
rm -rf "$stage"
mkdir -p "$stage"
for bin in imsg-extract imsg-server; do
  install -m 755 "${bindir}/${bin}" "${stage}/${bin}"
done
cp README.md "$stage/"
tar -C dist -czf "dist/${name}.tar.gz" "$name"
rm -rf "$stage"
(cd dist && shasum -a 256 "${name}.tar.gz" > "${name}.tar.gz.sha256")
echo "dist/${name}.tar.gz"
