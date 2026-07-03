# rgfile

[![CI](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Maymall/gigafile-rust-cli/actions/workflows/ci.yml)
[![License: GPL-3.0-only](https://img.shields.io/badge/License-GPL--3.0--only-blue.svg)](LICENSE)

`rgfile` is a fast, robust command-line client for [GigaFile.nu](https://gigafile.nu):
upload and download files straight from the terminal.

## Features

- **Single-file and multi-file downloads** — handles both individual file pages
  and multi-file *matomete* pages.
- **Password-protected links** — supply the key with `--key` (alias
  `--password`); it is sent to the server as `dlkey` and never written to disk.
- **Resumable downloads** — a `.part` file plus a metadata sidecar let an
  interrupted transfer continue where it stopped. Completion is atomic (the file
  is renamed into place only after the full body is verified against the
  server-reported size). Use `--no-resume` to always start from zero.
- **Correct filenames** — real names are decoded from `Content-Disposition`,
  including RFC 5987 `filename*=` values, so UTF-8 and Japanese names survive
  intact even when the page masks the displayed name.
- **Windows-safe names** — filenames are sanitized so they are valid on Windows
  as well as Unix.
- **Constant-memory uploads** — files are streamed to the server in chunks, so
  peak memory stays around 10 MiB regardless of the configured chunk size.
- **Resilient transfers** — chunks are sent sequentially with per-chunk retry;
  `--timeout` measures a *stall* (idle) window, not the total transfer duration.
- **Optional upload verification** — after upload, the returned page is checked
  against the remote `Content-Length`. Skip it with `--no-verify`.
- **Lifetime selection** — choose how long an upload lives: 3, 5, 7, 14, 30, 60,
  or 100 days.
- **Machine-readable output** — `--json` prints one final JSON object per run.
- **Meaningful exit codes** — every failure maps to a stable, documented code
  (see [Exit codes](#exit-codes)).
- **Static Linux binary** — a fully static musl build is published for Linux.

## Install

### Prebuilt binaries

Download the archive for your platform from the
[latest release](https://github.com/Maymall/gigafile-rust-cli/releases/latest).
Assets are named `rgfile-<version>-<target>`:

| Platform | Target |
|---|---|
| Linux x86_64 (glibc) | `x86_64-unknown-linux-gnu` |
| Linux x86_64 (static musl) | `x86_64-unknown-linux-musl` |
| macOS (Apple silicon) | `aarch64-apple-darwin` |
| macOS (Intel) | `x86_64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

Each release also ships `SHA256SUMS`; verify your download before use:

```bash
sha256sum -c SHA256SUMS
```

### From source

Requires Rust 1.85 or newer (MSRV 1.85):

```bash
cargo install --git https://github.com/Maymall/gigafile-rust-cli
```

## Usage

### Download

Download a page into the current directory:

```bash
rgfile download https://23.gigafile.nu/0123abcd-000000example
```

Choose an output directory, or an explicit filename for a single-file page:

```bash
rgfile download https://23.gigafile.nu/0123abcd-000000example -o ./downloads
rgfile download https://23.gigafile.nu/0123abcd-000000example -o "./example file.bin"
```

Provide a key for a password-protected link:

```bash
rgfile download https://23.gigafile.nu/0123abcd-000000example --key EXAMPLE-KEY-0000
```

If a page needs a key and none is given, `rgfile` prompts once (without echoing)
on an interactive terminal; non-interactive runs exit with code 15.

Emit a single JSON object for scripting:

```bash
rgfile download --json https://23.gigafile.nu/0123abcd-000000example
```

### Upload

Upload a file and print the resulting public download URL:

```bash
rgfile upload ./example-file.bin
```

Pick a lifetime (default 100 days; one of 3, 5, 7, 14, 30, 60, 100):

```bash
rgfile upload ./example-file.bin --lifetime 7
```

Tune the streaming chunk size (default `100MiB`; accepts a `K`/`M`/`G` suffix,
from 1 MiB up to 1 GiB):

```bash
rgfile upload ./example-file.bin --chunk-size 50M
```

Skip post-upload verification:

```bash
rgfile upload ./example-file.bin --no-verify
```

Emit a single JSON object for scripting:

```bash
rgfile upload --json ./example-file.bin
```

## Exit codes

`0` indicates success. Failures use the following codes:

| Code | Name | Meaning |
|---:|---|---|
| 2 | `usage` | Invalid CLI arguments or unsupported option value. |
| 10 | `invalid_url` | The URL is not a supported GigaFile download page. |
| 11 | `network` | Network request, timeout, or retry exhaustion failure. |
| 12 | `http_status` | Unexpected non-retryable HTTP status. |
| 13 | `parse` | Required page data could not be parsed. |
| 14 | `not_found_or_expired` | The file was not found or has expired. |
| 15 | `key_required` | A download key is required but was not available. |
| 16 | `password_wrong` | The download key was rejected. |
| 17 | `size_mismatch` | Downloaded size did not match the server header. |
| 18 | `io` | Local filesystem error. |
| 19 | `upload_rejected` | The upload endpoint rejected the upload. |
| 20 | `verify_failed` | Upload verification found a size mismatch. |

## Behavior and limits

- **Sequential by design.** Both downloads (including every file of a matomete
  page) and upload chunks are processed one at a time. This is deliberate — the
  tool aims to be gentle to the service rather than maximize throughput. For a
  matomete page, a failing file does not stop the others; the process exit code
  is the first failure encountered.
- **Uploads do not resume across runs.** Resume applies to downloads only. If an
  upload run fails, the next attempt re-uploads the whole file.
- **No bypass, no brute force, no scraping.** `rgfile` does not circumvent
  GigaFile restrictions, does not guess or brute-force passwords, and does not
  crawl or scrape links.

## License and acknowledgements

`rgfile` is licensed under **GPL-3.0-only**; see [LICENSE](LICENSE).

It is derived from the GPL-licensed Python projects
[`Sraq-Zit/gfile`](https://github.com/Sraq-Zit/gfile) and
[`fireattack/gfile`](https://github.com/fireattack/gfile); see
[NOTICE.md](NOTICE.md) for details. The corresponding source for any binary
release is this repository at the matching release tag.
