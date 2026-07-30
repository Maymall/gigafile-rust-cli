#!/bin/sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
test_root=$(mktemp -d)
escaped_name="rgfile-installer-escaped-$$"
cleanup() {
    rm -rf "$test_root"
    rm -f "$test_root/runtime/$escaped_name"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

version=9.9.9
target=x86_64-unknown-linux-musl
asset="rgfile-$version-$target.tar.gz"
member="rgfile-$version-$target/rgfile"

mkdir -p "$test_root/mockbin" "$test_root/runtime"

cat >"$test_root/mockbin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf '%s\n' Linux ;;
    -m) printf '%s\n' x86_64 ;;
    *) exit 1 ;;
esac
EOF
chmod 755 "$test_root/mockbin/uname"

cat >"$test_root/mockbin/curl" <<'EOF'
#!/bin/sh
set -eu
output=
write_out=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output | --write-out | --proto | --proto-redir | --connect-timeout | \
            --max-time | --max-redirs | --retry | --max-filesize)
            option=$1
            value=$2
            shift 2
            case "$option" in
                --output) output=$value ;;
                --write-out) write_out=$value ;;
            esac
            ;;
        --fail | --silent | --show-error | --location | --head | --tlsv1.2)
            shift
            ;;
        *)
            url=$1
            shift
            ;;
    esac
done

case "$url" in
    https://github.com/Maymall/gigafile-rust-cli/releases/latest)
        [ "$write_out" = '%{url_effective}' ]
        printf '%s' 'https://github.com/Maymall/gigafile-rust-cli/releases/tag/v9.9.9'
        ;;
    */SHA256SUMS)
        cp "$FIXTURE_SUMS" "$output"
        ;;
    */rgfile-9.9.9-x86_64-unknown-linux-musl.tar.gz)
        cp "$FIXTURE_ASSET" "$output"
        ;;
    *)
        printf 'unexpected curl URL: %s\n' "$url" >&2
        exit 1
        ;;
esac
EOF
chmod 755 "$test_root/mockbin/curl"

create_archive() {
    archive_kind=$1
    archive_path=$2
    python3 - "$archive_kind" "$archive_path" "$member" "$escaped_name" <<'PY'
import io
import sys
import tarfile

kind, archive_path, member, escaped_name = sys.argv[1:]
binary = b"#!/bin/sh\nprintf '%s\\n' 'rgfile 9.9.9'\n"


def add_regular(archive, name, body):
    entry = tarfile.TarInfo(name)
    entry.mode = 0o755
    entry.size = len(body)
    archive.addfile(entry, io.BytesIO(body))


with tarfile.open(archive_path, "w:gz") as archive:
    if kind == "valid":
        add_regular(archive, member, binary)
        add_regular(archive, f"../{escaped_name}", b"must not be extracted")
    elif kind == "duplicate":
        add_regular(archive, member, binary)
        add_regular(archive, member, b"different duplicate")
    elif kind == "symlink":
        entry = tarfile.TarInfo(member)
        entry.type = tarfile.SYMTYPE
        entry.linkname = f"../../{escaped_name}"
        archive.addfile(entry)
    elif kind == "hardlink":
        entry = tarfile.TarInfo(member)
        entry.type = tarfile.LNKTYPE
        entry.linkname = f"../../{escaped_name}"
        archive.addfile(entry)
    else:
        raise SystemExit(f"unknown archive kind: {kind}")
PY
}

write_checksums() {
    archive_path=$1
    sums_path=$2
    python3 - "$archive_path" "$sums_path" "$asset" <<'PY'
import hashlib
import pathlib
import sys

archive_path, sums_path, asset = sys.argv[1:]
digest = hashlib.sha256(pathlib.Path(archive_path).read_bytes()).hexdigest()
pathlib.Path(sums_path).write_text(f"{digest}  {asset}\n", encoding="ascii")
PY
}

run_installer() {
    archive_path=$1
    sums_path=$2
    install_dir=$3
    FIXTURE_ASSET=$archive_path \
        FIXTURE_SUMS=$sums_path \
        HOME="$test_root/home" \
        PATH="$test_root/mockbin:$PATH" \
        RGFILE_INSTALL_DIR=$install_dir \
        TMPDIR="$test_root/runtime" \
        sh "$repo_dir/install.sh"
}

valid_archive="$test_root/valid.tar.gz"
valid_sums="$test_root/valid.SHA256SUMS"
valid_install="$test_root/valid-install"
create_archive valid "$valid_archive"
write_checksums "$valid_archive" "$valid_sums"
run_installer "$valid_archive" "$valid_sums" "$valid_install"
[ "$("$valid_install/rgfile" --version)" = "rgfile 9.9.9" ]
[ ! -e "$test_root/runtime/$escaped_name" ]

for archive_kind in duplicate symlink hardlink; do
    invalid_archive="$test_root/$archive_kind.tar.gz"
    invalid_sums="$test_root/$archive_kind.SHA256SUMS"
    invalid_install="$test_root/$archive_kind-install"
    create_archive "$archive_kind" "$invalid_archive"
    write_checksums "$invalid_archive" "$invalid_sums"
    mkdir -p "$invalid_install"
    printf '%s\n' "previous installation" >"$invalid_install/rgfile"

    if run_installer \
        "$invalid_archive" \
        "$invalid_sums" \
        "$invalid_install" \
        >"$test_root/$archive_kind.stdout" \
        2>"$test_root/$archive_kind.stderr"; then
        printf 'installer unexpectedly accepted a %s archive\n' "$archive_kind" >&2
        exit 1
    fi
    [ "$(cat "$invalid_install/rgfile")" = "previous installation" ]
done

printf '%s\n' "POSIX installer archive safety tests passed."
