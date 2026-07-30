# rgfile

[![CI](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rgfile.svg)](https://crates.io/crates/rgfile)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A command-line client for [GigaFile.nu](https://gigafile.nu).

English | [简体中文](docs/README.zh.md) | [日本語](docs/README.ja.md)

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.sh | sh   # Linux / macOS
cargo install rgfile                                                                          # Rust 1.88+
brew install Maymall/tap/rgfile                                                               # Homebrew
```

Windows: `irm https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.ps1 | iex`

The one-line installers verify the selected archive against the release's
`SHA256SUMS` before extracting it. `install.sh` supports Linux x86_64 (static
musl) and macOS x86_64/arm64; `install.ps1` supports Windows x86_64. The
[releases page](https://github.com/Maymall/gigafile-rust-cli/releases/latest)
also provides a Linux x86_64 glibc archive and an x86_64 Debian package. Other
architectures can build from source.
Release-installed binaries upgrade themselves with `rgfile self-update`.

## Usage

```bash
rgfile ul file.bin                   # upload; prints the URL, delete key, expiry
rgfile ul file.bin --lifetime 7      # keep for 7 days (3, 5, 7, 14, 30, 60, or 100)

rgfile dl <url>                      # download; interrupted transfers resume
rgfile dl <url> --threads 8          # segmented download over several connections
rgfile dl <url> --select 1,3-5       # pick files from a multi-file page

rgfile info <url>                    # inspect a page without downloading
rgfile delete <url>                  # prompt privately for the delete key on a TTY
rgfile delete <url> --delkey-file key.txt --yes
rgfile parts list                    # leftover partial downloads
rgfile parts clean --older-than 7    # drop stale ones; active downloads are never touched

rgfile config init                   # interactive configuration
rgfile history list                  # local history (opt-in)
rgfile completions zsh               # shell completions
```

Transfer, inspection, history-list, parts-list/clean, config-show, and delete
commands accept `--json`; destructive JSON commands also require `--yes`.
`rgfile <command> --help` has the complete option list.

## Configuration

Optional TOML at `~/.config/rgfile/config.toml`
(macOS: `~/Library/Application Support/rgfile/`, Windows: `%APPDATA%\rgfile\`).
CLI flags override it.

```toml
[download]
dir = "/home/alice/Downloads"
threads = 8                    # connections per file, 1–16

[upload]
lifetime = 7                   # days: 3/5/7/14/30/60/100
threads = 4                    # read-ahead chunk window, 1–16

[network]
timeout = 60                   # idle seconds per request/read, 1–86400
retries = 3                    # retryable failures, 0–20
# user_agent = "rgfile/0.10.1"

[history]
enabled = true                 # off by default
store_delete_keys = false      # plaintext, opt-in
```

## Behavior

- Downloads resume from where they stopped, even when the page masks the file
  name; completion is atomic and size-checked. Names come from
  `Content-Disposition`, so UTF-8 survives.
- Segmented downloads keep a few connections active and back off when the
  server pushes back, instead of hammering it.
- Ctrl-C prints how much reached disk and how to resume. Delete keys and
  download passwords never appear in logs.
- Uploads stream with per-chunk retry; chunk completion stays ordered because
  the server drops out-of-order chunks (verified against the live service).
- Upload read-ahead is bounded to roughly 512 MiB; a large chunk size can reduce
  the effective window below `--threads` to keep memory predictable.
- rgfile does not bypass GigaFile restrictions, guess passwords, or scrape links.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 2 | Invalid arguments |
| 10 | Not a GigaFile URL |
| 11 | Network failure, retries exhausted |
| 12 | Unexpected HTTP status or oversized control response |
| 13 | Page could not be parsed |
| 14 | Not found or expired |
| 15 / 16 | Download key required / rejected |
| 17 | Size mismatch |
| 18 | Filesystem error, or the target already exists (`--force` overwrites) |
| 19 | Upload rejected |
| 20 | Verification failed |
| 21 | Target locked by another rgfile process |
| 22 | Delete rejected |
| 130 | Interrupted; the kept `.part` resumes on re-run |

Changelog: [docs/CHANGELOG.md](docs/CHANGELOG.md)

History is disabled by default. When enabled, records are stored as JSONL in
the platform data directory (`~/.local/share/rgfile/` on Linux). Delete keys
are never stored unless `history.store_delete_keys = true`; that opt-in keeps
them locally in plaintext and should be treated like a credential.
`history list --json` never prints stored delete keys.

## License

MIT — see [LICENSE](LICENSE).

The GigaFile.nu protocol flow was originally worked out with reference to
[`Sraq-Zit/gfile`](https://github.com/Sraq-Zit/gfile) and its fork
[`fireattack/gfile`](https://github.com/fireattack/gfile). Thanks!
