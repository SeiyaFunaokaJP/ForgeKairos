#!/usr/bin/env bash
# Runs script/bundle-linux inside the container that `script/bundle.ps1 -Os linux`
# starts: the checkout is mounted at /src and CARGO_TARGET_DIR is a volume, so
# the results are copied out to /src/target.

set -euo pipefail

# The checkout belongs to the host's user and git refuses to read a repository
# owned by someone else. Older gits do not know "*", hence the explicit paths.
git config --global --add safe.directory '*'
git config --global --add safe.directory /src
if [[ -n "${GIT_DIR:-}" ]]; then
    git config --global --add safe.directory "${GIT_DIR}"
fi

cd /src
script/bundle-linux "$@"

arch=$(uname -m)
package="forgekairos-linux-${arch}.tar.gz"
remote_server="forgekairos-remote-server-linux-${arch}.gz"
mkdir -p /src/target
cp "${CARGO_TARGET_DIR}/release/${package}" "${CARGO_TARGET_DIR}/${remote_server}" /src/target/

# On a Linux host the files written here as root would stay root-owned in the
# user's checkout. A Windows drive mounted into WSL ignores ownership.
owner=$(stat -c %u:%g /src)
if [[ "$owner" != "0:0" ]]; then
    chown "$owner" /src/target "/src/target/${package}" "/src/target/${remote_server}" \
        /src/assets/licenses.md 2>/dev/null || true
fi

echo "Copied ${package} and ${remote_server} to target/"
