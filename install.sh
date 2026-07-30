#!/bin/sh
# rgfile installer: downloads the latest release binary for this platform,
# verifies its SHA-256 against the release's SHA256SUMS, and installs it.
#
#   curl -fsSL https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.sh | sh
#
# Override the install directory with RGFILE_INSTALL_DIR (default: ~/.local/bin).
set -eu

REPO="Maymall/gigafile-rust-cli"
INSTALL_DIR="${RGFILE_INSTALL_DIR:-$HOME/.local/bin}"
MAX_ARCHIVE_BYTES=536870912
MAX_CHECKSUM_BYTES=1048576
MAX_BINARY_BYTES=268435456
# POSIX `ulimit -f` uses 512-byte blocks.
MAX_BINARY_BLOCKS=524288

say() { printf '%s\n' "$*"; }
err() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

lower_file_size_limit() {
    requested_blocks=$1
    current_blocks=$(ulimit -f)
    case "$current_blocks" in
        unlimited) ulimit -f "$requested_blocks" ;;
        *[!0-9]* | '') return 1 ;;
        *)
            if [ "$current_blocks" -gt "$requested_blocks" ]; then
                ulimit -f "$requested_blocks"
            fi
            ;;
    esac
}

download() {
    url=$1
    output=$2
    max_bytes=$3
    max_blocks=$(((max_bytes + 511) / 512))
    (
        lower_file_size_limit "$max_blocks"
        curl \
            --fail \
            --silent \
            --show-error \
            --location \
            --proto '=https' \
            --proto-redir '=https' \
            --tlsv1.2 \
            --connect-timeout 15 \
            --max-time 600 \
            --max-redirs 5 \
            --retry 3 \
            --max-filesize "$max_bytes" \
            --output "$output" \
            "$url"
    ) || return 1
    downloaded_size=$(wc -c <"$output")
    [ "$downloaded_size" -le "$max_bytes" ]
}

extract_release_binary() {
    archive=$1
    member=$2
    output=$3
    member_list=$4
    member_details=$5

    # Ask tar about the exact trusted member name, not the whole archive. The
    # listing must contain that name exactly once and the entry must be a
    # regular file. This rejects duplicate, symlink, and hard-link entries.
    (
        lower_file_size_limit 2048
        LC_ALL=C tar -tzf "$archive" "$member" >"$member_list"
    ) || err "cannot inspect the expected archive member"
    [ "$(cat "$member_list")" = "$member" ] ||
        err "archive must contain exactly one $member entry"

    (
        lower_file_size_limit 2048
        LC_ALL=C tar -tvzf "$archive" "$member" >"$member_details"
    ) || err "cannot inspect the expected archive member type"
    entry_type=$(
        awk '
            NR == 1 { type = substr($1, 1, 1) }
            END {
                if (NR == 1) {
                    print type
                } else {
                    exit 1
                }
            }
        ' "$member_details"
    ) || err "archive must contain exactly one $member entry"
    [ "$entry_type" = "-" ] || err "archive member $member is not a regular file"

    # Stream only the selected regular file to a path chosen by this script.
    # The file-size limit bounds decompression even for a gzip bomb.
    if ! (
        lower_file_size_limit "$MAX_BINARY_BLOCKS"
        LC_ALL=C tar -xOzf "$archive" "$member" >"$output"
    ); then
        rm -f "$output"
        err "cannot safely extract $member"
    fi
    binary_size=$(wc -c <"$output")
    if [ "$binary_size" -le 0 ] || [ "$binary_size" -gt "$MAX_BINARY_BYTES" ]; then
        err "archive binary has an invalid size"
    fi
}

command -v curl >/dev/null 2>&1 || err "curl is required"
command -v tar >/dev/null 2>&1 || err "tar is required"
command -v awk >/dev/null 2>&1 || err "awk is required"
command -v cat >/dev/null 2>&1 || err "cat is required"
command -v install >/dev/null 2>&1 || err "install is required"
command -v mv >/dev/null 2>&1 || err "mv is required"
command -v wc >/dev/null 2>&1 || err "wc is required"

os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Linux)
        case "$arch" in
            # The static musl build runs on any Linux regardless of libc.
            x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
            *) err "unsupported Linux architecture: $arch — download a release archive manually" ;;
        esac
        ;;
    Darwin)
        case "$arch" in
            arm64) target="aarch64-apple-darwin" ;;
            x86_64) target="x86_64-apple-darwin" ;;
            *) err "unsupported macOS architecture: $arch — download a release archive manually" ;;
        esac
        ;;
    *)
        err "unsupported OS: $os — on Windows use install.ps1, otherwise download a release archive manually"
        ;;
esac

# Resolve the latest tag from the releases/latest redirect (no API rate limit).
latest_url=$(
    curl \
        --fail \
        --silent \
        --show-error \
        --location \
        --head \
        --output /dev/null \
        --write-out '%{url_effective}' \
        --proto '=https' \
        --proto-redir '=https' \
        --tlsv1.2 \
        --connect-timeout 15 \
        --max-time 60 \
        --max-redirs 5 \
        --retry 3 \
        "https://github.com/$REPO/releases/latest"
) ||
    err "cannot resolve the latest release"
release_prefix="https://github.com/$REPO/releases/tag/"
case "$latest_url" in
    "$release_prefix"*) tag=${latest_url#"$release_prefix"} ;;
    *) err "latest release redirected outside the expected repository: $latest_url" ;;
esac
printf '%s\n' "$tag" |
    awk '/^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/ { valid = 1 }
         END { exit(valid ? 0 : 1) }' ||
    err "cannot determine the latest version (got tag: $tag)"
version=${tag#v}

asset="rgfile-$version-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
stage=
cleanup() {
    rm -rf "$tmp"
    if [ -n "$stage" ]; then
        rm -f "$stage"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

say "Downloading rgfile $version for $target ..."
download "$base/$asset" "$tmp/$asset" "$MAX_ARCHIVE_BYTES" ||
    err "download failed: $base/$asset"
download "$base/SHA256SUMS" "$tmp/SHA256SUMS" "$MAX_CHECKSUM_BYTES" ||
    err "download failed: $base/SHA256SUMS"

# Match exactly one complete asset field. Treat SHA256SUMS as untrusted input.
expected_hash=$(
    awk -v asset="$asset" '
        $2 == asset {
            asset_lines++
            if (NF == 2 && length($1) == 64 && $1 !~ /[^0-9A-Fa-f]/) {
                hash = tolower($1)
                valid_lines++
            }
        }
        END {
            if (asset_lines == 1 && valid_lines == 1) {
                print hash
            } else {
                exit 1
            }
        }
    ' "$tmp/SHA256SUMS"
) || err "expected exactly one checksum for $asset in SHA256SUMS"

if command -v sha256sum >/dev/null 2>&1; then
    actual_hash=$(sha256sum "$tmp/$asset" | awk '{ print tolower($1) }')
elif command -v shasum >/dev/null 2>&1; then
    actual_hash=$(shasum -a 256 "$tmp/$asset" | awk '{ print tolower($1) }')
else
    err "need sha256sum or shasum to verify the download"
fi
[ "$actual_hash" = "$expected_hash" ] || err "checksum verification FAILED"
say "Checksum OK."

member="rgfile-$version-$target/rgfile"
binary="$tmp/rgfile"
extract_release_binary \
    "$tmp/$asset" \
    "$member" \
    "$binary" \
    "$tmp/member-list" \
    "$tmp/member-details"
mkdir -p "$INSTALL_DIR"
stage=$(mktemp "$INSTALL_DIR/.rgfile.XXXXXX") || err "cannot create a staging file in $INSTALL_DIR"
install -m 755 "$binary" "$stage" ||
    err "cannot stage the binary in $INSTALL_DIR"
staged_version=$("$stage" --version 2>/dev/null) ||
    err "staged binary failed its --version check"
[ "$staged_version" = "rgfile $version" ] ||
    err "staged binary reported an unexpected version: $staged_version"
mv -f "$stage" "$INSTALL_DIR/rgfile" || err "cannot replace $INSTALL_DIR/rgfile"
stage=

say "Installed: $INSTALL_DIR/rgfile ($("$INSTALL_DIR/rgfile" --version))"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say ""
        say "NOTE: $INSTALL_DIR is not in your PATH. Add this to your shell profile:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
