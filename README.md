# rgfile

[![CI](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rgfile.svg)](https://crates.io/crates/rgfile)
[![License: GPL-3.0-only](https://img.shields.io/badge/License-GPL--3.0--only-blue.svg)](LICENSE)

`rgfile` is a fast, robust command-line client for [GigaFile.nu](https://gigafile.nu):
upload, download, inspect, and delete shares straight from the terminal. It
began as a Rust rewrite of the Python `gfile` tools and has grown into a
full-featured client in its own right — resumable segmented downloads, share
management, and protocol behavior verified against the live service.

## Features

**Downloads**

- Single-file and multi-file (matomete) pages; `--key` for password-protected
  links, `--select 1,3-5` to pick files by `rgfile info` index
- Resumable: interrupted transfers continue where they stopped — including when
  the page masks the display name; completion is atomic and size-verified
- Segmented multi-connection mode (`--threads 1-16`) with one overall progress
  bar plus per-connection child bars, each with its own speed
- Ctrl-C prints how much reached disk and where the kept `.part` lives;
  `rgfile parts list` / `parts clean` inspect and remove leftovers safely
  (an active download's files are never touched)
- Correct filenames decoded from `Content-Disposition` (RFC 5987): UTF-8 /
  Japanese names survive intact even when the page shows a masked name

**Uploads & share management**

- Streaming chunked uploads with per-chunk retry and near-constant memory;
  optional read-ahead window (`--threads`) keeps chunk completion ordered
- Results include the download URL, delete key, and estimated expiry;
  lifetime selectable (3–100 days)
- `rgfile delete <url>` removes an uploaded share with its delete key

**Workflow**

- `rgfile info <url>` inspects a page without downloading
- `dl` / `ul` aliases, shell completions, `--json` output and stable exit
  codes for scripting
- Interactive `rgfile config init` wizard, TOML config, opt-in local history
- `rgfile self-update` with SHA-256 verification; static musl Linux binary,
  plus macOS (arm64/Intel) and Windows builds

## Install

One-liner (Linux / macOS; verifies SHA-256, installs to `~/.local/bin`):

```bash
curl -fsSL https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.ps1 | iex
```

Other options:

```bash
cargo install rgfile                 # crates.io, needs Rust 1.85+
brew install Maymall/tap/rgfile      # Homebrew (macOS / Linux)
```

`cargo install` places the binary in `~/.cargo/bin`; if your shell can't find
`rgfile` afterwards, add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell
profile.

Debian / Ubuntu: download `rgfile_<version>_amd64.deb` from the
[latest release](https://github.com/Maymall/gigafile-rust-cli/releases/latest)
and `sudo apt install ./rgfile_<version>_amd64.deb`. Release archives for all
platforms (with `SHA256SUMS`) are on the same page.

### Upgrade

Rerun the install one-liner, or `cargo install rgfile`, or
`brew upgrade rgfile` — whichever you installed with. Release-installed
binaries can also run `rgfile self-update`.

## Usage

```bash
# Download (alias: dl)
rgfile download https://23.gigafile.nu/0123abcd-000000example
rgfile dl https://23.gigafile.nu/0123abcd-000000example
rgfile download https://23.gigafile.nu/0123abcd-000000example -o ./downloads
rgfile download https://23.gigafile.nu/0123abcd-000000example --key EXAMPLE-KEY-0000
rgfile download --select 1,3-5 https://23.gigafile.nu/0123abcd-000000example
rgfile download --json https://23.gigafile.nu/0123abcd-000000example

# Upload (alias: ul; prints the download URL and the delete key)
rgfile upload ./example-file.bin
rgfile ul ./example-file.bin
rgfile upload ./example-file.bin --lifetime 7
rgfile upload ./example-file.bin --threads 4
rgfile upload --json ./example-file.bin

# Delete an uploaded file by URL and delete key
rgfile delete https://23.gigafile.nu/0123abcd-000000example --delkey EXA1
rgfile delete https://23.gigafile.nu/0123abcd-000000example --yes

# Inspect a page without downloading
rgfile info https://23.gigafile.nu/0123abcd-000000example

# Inspect or clean leftover partial downloads
rgfile parts list ./downloads
rgfile parts clean ./downloads --older-than 7 --yes

# Generate shell completions
rgfile completions zsh > _rgfile
```

If a page needs a key and none is given, `rgfile` prompts on an interactive
terminal; non-interactive runs exit with code 15. See `rgfile <command> --help`
for all options (`--timeout`, `--retries`, `--no-resume`, `--chunk-size`, ...).

## Configuration

Optional TOML file at `~/.config/rgfile/config.toml` (Linux),
`~/Library/Application Support/rgfile/config.toml` (macOS), or
`%APPDATA%\rgfile\config.toml` (Windows). CLI options override config values;
`--config <path>` loads a specific file, `--no-config` skips loading.

Use `rgfile config init` to create a config interactively, or
`rgfile config init --defaults` to write a commented defaults template without
prompting. `rgfile config show` prints the effective values and where each one
came from; add `--json` for machine-readable output. `rgfile config path`
prints the path rgfile will use, even before the file exists.

```toml
[download]
dir = "/home/alice/Downloads"  # default output directory
threads = 1                    # connections per file, 1-16 (see note below)

[upload]
lifetime = 7                   # default lifetime in days: 3/5/7/14/30/60/100
threads = 1                    # read-ahead chunk window, 1-16 (see note below)

[network]
timeout = 60                   # idle timeout in seconds
retries = 3

[history]
enabled = false                # opt-in local history
store_delete_keys = false      # keep upload delete keys in history (plaintext)
```

## History

Off by default. Enable with `history.enabled = true` (or `--history` for one
command). Records go to `~/.local/share/rgfile/history.jsonl` (platform
equivalent): timestamp, operation, URL, file names, bytes, result.

```bash
rgfile history list
rgfile history clear
```

Download passwords are never stored. Upload delete keys are stored only if you
opt in with `history.store_delete_keys = true`. `rgfile delete` can use a stored
delete key when history is enabled and the URL matches a previous upload record;
otherwise pass `--delkey`.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 2 | Invalid CLI arguments or option value |
| 10 | Not a supported GigaFile URL |
| 11 | Network failure / timeout / retries exhausted |
| 12 | Unexpected HTTP status |
| 13 | Page could not be parsed |
| 14 | File not found or expired |
| 15 | Download key required |
| 16 | Download key rejected |
| 17 | Downloaded size mismatch |
| 18 | Local filesystem error |
| 19 | Upload rejected by the server |
| 20 | Upload or self-update verification failed |
| 21 | Download target is already locked by another rgfile process |
| 22 | Delete request rejected by the server |
| 130 | Interrupted with Ctrl-C; a summary shows the kept `.part` for resume |

## Notes

- Downloads use one connection by default. `download --threads N` /
  `download.threads` enables segmented downloads with one overall progress bar
  and per-connection child bars. If GigaFile answers ranged requests with the
  full file, rgfile automatically continues on a single connection.
- Upload chunks must complete in order. Live protocol probing showed that
  out-of-order chunk completion can drop data, so `upload --threads N` /
  `upload.threads` uses a read-ahead pipeline: it may keep up to N chunks in
  memory while still sending and completing chunks sequentially. Default
  `threads = 1` keeps the streaming one-chunk behavior; higher values can use
  roughly `N * chunk-size` memory plus HTTP overhead.
- Uploads cannot resume across runs; a failed upload restarts from scratch.
- `parts clean` never removes a group whose `.part.json.lock` is currently held
  by another rgfile process.
- rgfile does not bypass GigaFile restrictions, guess passwords, or scrape links.

## License

GPL-3.0-only; see [LICENSE](LICENSE). Derived from the GPL-licensed Python
projects [`Sraq-Zit/gfile`](https://github.com/Sraq-Zit/gfile) and
[`fireattack/gfile`](https://github.com/fireattack/gfile) — see
[NOTICE.md](NOTICE.md). Corresponding source for a binary release is this
repository at the matching tag.
