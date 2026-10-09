# The files that script/bundle-linux and script/bundle-mac ship for remote
# hosts, as script/lib/remote-artifacts.ps1 does for the Windows installer:
# crates/remote uploads the Linux remote server and slirp4netns to SSH, WSL and
# Docker hosts, because this fork blocks the download that would replace them.
#
# Sourced by bash scripts that run with `set -euo pipefail`.

# The version script/lib/remote-artifacts.ps1 downloads, with the SHA-256 sums
# from that release's SHA256SUMS. License: slirp4netns is GPLv2+, its bundled
# libslirp MIT/BSD.
slirp4netns_version="v1.2.3"

slirp4netns_sha256() {
    case "$1" in
        x86_64) echo "2051ae25baba59c96f42bf2b7b7efa2eabb022e497b6c4b692b1428d2b2d1f5b" ;;
        aarch64) echo "289b0838f849da0d6d5de596e6c5010fbb134a2912d3cc32a0e2e2b759543278" ;;
        *)
            echo "No slirp4netns is published for $1" >&2
            return 1
            ;;
    esac
}

file_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# download_slirp4netns <arch> <destination>: the static slirp4netns for a Linux
# host of <arch>, downloaded once and checked against its published sum.
download_slirp4netns() {
    local arch="$1"
    local destination="$2"
    local expected
    expected=$(slirp4netns_sha256 "$arch")
    if [[ -f "$destination" && "$(file_sha256 "$destination")" == "$expected" ]]; then
        echo "slirp4netns is already downloaded: $destination"
        return
    fi
    mkdir -p "$(dirname "$destination")"
    local download="${destination}.download"
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
        "https://github.com/rootless-containers/slirp4netns/releases/download/${slirp4netns_version}/slirp4netns-${arch}" \
        --output "$download"
    local actual
    actual=$(file_sha256 "$download")
    if [[ "$actual" != "$expected" ]]; then
        rm -f "$download"
        echo "slirp4netns-${arch} ${slirp4netns_version} has SHA-256 ${actual}, expected ${expected}" >&2
        return 1
    fi
    chmod +x "$download"
    mv -f "$download" "$destination"
}

# build_linux_remote_server_with_zigbuild <cargo build flag> <features flag> <destination>:
# cross-compiles remote_server for x86_64 Linux (static musl, so any glibc) with
# cargo-zigbuild, as script/lib/remote-artifacts.ps1 does on Windows, and gzips
# it to <destination>. Its own target directory keeps the editor's build from
# being invalidated.
build_linux_remote_server_with_zigbuild() {
    local build_flag="$1"
    local features_flag="$2"
    local destination="$3"
    local triple="x86_64-unknown-linux-musl"
    local target_dir="target/remote_server"
    local profile="release"
    if [[ "$build_flag" != "--release" ]]; then
        profile="debug"
    fi

    if ! command -v zig >/dev/null 2>&1; then
        echo "zig is needed to cross-compile the Linux remote server (brew install zig)." >&2
        return 1
    fi
    if ! command -v cargo-zigbuild >/dev/null 2>&1; then
        cargo install cargo-zigbuild --locked
    fi
    rustup target add "$triple"

    # The bundle config carries the static musl flags. strip=debuginfo keeps
    # the symbol names for backtraces but not the DWARF, which a remote host
    # would otherwise store for every version.
    # shellcheck disable=SC2086 # the flags are empty or one word each
    CARGO_PROFILE_RELEASE_STRIP=debuginfo CARGO_PROFILE_DEV_STRIP=debuginfo \
        cargo zigbuild --config .cargo/bundle-config.toml ${build_flag} ${features_flag} \
        --package remote_server --target "$triple" --target-dir "$target_dir"

    mkdir -p "$(dirname "$destination")"
    gzip -f --stdout --best "${target_dir}/${triple}/${profile}/remote_server" > "$destination"
}
