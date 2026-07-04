# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows Semantic
Versioning.

## [Unreleased]

## [0.9.2] - 2026-07-04

### Fixed

- Write download sidecars atomically. The Ctrl-C summary could read a half-written sidecar and fall back to the preallocated `.part` length, falsely reporting 100% for a partial segmented download; unreadable sidecars now report unknown progress instead of guessing.
- Stop counting preset resume positions as instantaneous transfer, which showed absurd `TiB/s` speeds on resumed or already-completed segments.
- Route log lines through the active progress display and clear dropped progress groups, so warnings and retries no longer leave stacked stale progress frames on screen.

### Added

- Show a percentage on the overall download and upload progress bars.

## [0.9.1] - 2026-07-04

### Fixed

- Re-probe partial-download locks at deletion time in `parts clean`, so a download that starts while the confirmation prompt is open never loses its files.

## [0.9.0] - 2026-07-04

### Added

- Add `rgfile delete` for removing an uploaded file with its delete key, including optional history lookup for stored delete keys.
- Add `rgfile parts list` and `rgfile parts clean` to inspect and safely remove leftover `.part` files, sidecars, and stale locks.

## [0.8.1] - 2026-07-04

### Changed

- Print a summary when a download is interrupted with Ctrl-C: percent complete, bytes on disk, the kept `.part` path, and a resume hint; the process now exits with code 130.

## [0.8.0] - 2026-07-04

### Added

- Add `dl` and `ul` as visible aliases for the `download` and `upload` subcommands.

## [0.7.1] - 2026-07-04

### Fixed

- Resume sequential and segmented downloads when a masked page display name is replaced by a Content-Disposition filename.
- Use second-precise ETA on single-bar download and upload progress.

## [0.7.0] - 2026-07-04

### Added

- Add `rgfile config path`, `rgfile config show`, and `rgfile config init` for inspecting and creating configuration files.
- Add interactive config creation for common settings plus `config init --defaults` for a commented template.
- Show per-connection speeds on segmented download child progress bars and use second-precise ETA on the main progress bar.

## [0.6.0] - 2026-07-04

### Added

- Show segmented downloads with one overall progress bar plus per-connection child bars when `download --threads N` renders on a TTY.
- Add `upload --threads` and `[upload].threads` as an upload read-ahead window while preserving ordered chunk completion.

### Changed

- Document that live upload probing did not show safe out-of-order chunk completion, so higher upload thread counts prefetch chunks but still complete HTTP uploads serially.
- Clarify upload memory usage for read-ahead mode as roughly `threads * chunk-size`.

## [0.5.3] - 2026-07-04

### Fixed

- Prevent concurrent downloads targeting the same file from racing over the `.part` data and sidecar by taking a nonblocking OS advisory lock.
- Restore default Unix SIGPIPE handling so commands such as `rgfile completions bash | head` exit quietly when the pipe closes.
- Skip GitHub Release creation when a duplicate tag workflow run finds that the release already exists.

### Changed

- Clarify resume restart warnings when an existing sidecar is incompatible, including the common case of changing `--threads` between attempts.

## [0.5.2] - 2026-07-04

### Fixed

- Send the `Range` request header title-cased (`Range:` instead of `range:`). GigaFile's server matches header names case-sensitively and silently ignores the lowercase form, answering `200 OK` instead of `206 Partial Content` — this made every resumed and segmented (`--threads`) download fall back to a single connection or restart from zero against the real server, even though it worked fine in local mock-based tests.

## [0.5.1] - 2026-07-03

### Fixed

- Show the resolved real filename in the download progress bar instead of the page's masked display name.

### Changed

- Document the `~/.cargo/bin` PATH requirement for `cargo install` in the README.

## [0.5.0] - 2026-07-03

### Added

- Add `rgfile self-update` with latest-release redirect lookup, SHA-256 verification, and atomic binary replacement.
- Add `download --select <SPEC>` for 1-based matomete file indexes and include indexes in `rgfile info` output.
- Add `rgfile completions <shell>` for bash, zsh, fish, PowerShell, and Elvish.

## [0.4.0] - 2026-07-03

### Added

- Add experimental `--threads` / `download.threads` segmented download attempts with automatic single-connection fallback; live GigaFile currently declines parallel ranged downloads.
- Add segmented `.part` sidecar v2 metadata so interrupted ranged download attempts can resume from the first unfinished segment.
- Report the actual per-file download connection count in JSON output.

## [0.3.2] - 2026-07-03

### Added

- Include upload delete keys, remote filenames, and estimated expiration timestamps in upload results.
- Add `rgfile info` to inspect GigaFile pages without downloading file bodies.
- Add TOML configuration with platform default paths, `--config`, and `--no-config`.
- Add opt-in JSONL history recording plus `rgfile history list` and `rgfile history clear`.
- Add one-line install scripts (`install.sh` / `install.ps1`) that download the latest release, verify its SHA-256, and install the binary.
- Publish releases as a Debian package (`rgfile_<version>_amd64.deb`, static musl binary) alongside the archives.
- Publish the crate to crates.io (`cargo install rgfile`) and provide an AUR package (`rgfile-bin`).

## [0.3.1] - 2026-07-03

### Changed

- Rename the crate and binary from `gfile` to `rgfile`.
- Rename the repository to `gigafile-rust-cli`.
- Rewrite the README to present the project on its own terms.

## [0.3.0] - 2026-07-03

### Added

- Add GitHub release packaging for Linux, macOS, and Windows binaries.
- Add release checksum generation and GitHub Release notes with GPL attribution.
- Document release installation, migration behavior, timeout semantics, exit codes, and GPL compliance checklist.

### Changed

- Upload `--timeout` now measures idle activity while streaming a chunk or waiting for the response, instead of limiting the total chunk request duration.
- Upload progress now advances while the request body is streamed and resets to confirmed bytes when a chunk retry starts.
- Upload retry classification now uses structured retry metadata instead of matching error text.

## [0.2.0] - 2026-07-03

### Added

- Implement single-file uploads with serial streaming multipart chunks.
- Add upload landing-page parsing and local fixture coverage.
- Add upload verification via returned download page `Content-Length`.
- Add upload JSON output, CLI snapshots, and upload error coverage for rejected uploads and verification failures.

## [0.1.0] - 2026-07-03

### Added

- Bootstrap the Rust package, CLI shell, GPL compliance files, and CI workflow.
- Implement single-file and matomete downloads with sequential execution.
- Add download keys via `--key`, `--password`, and `-k`.
- Add resumable `.part` downloads with `--no-resume`.
- Add final `--json` output and CLI snapshot coverage.
- Add parser fixtures for matomete, password-required, wrong-key, missing, expired, and blocked pages.
- Preserve real UTF-8 filenames from `Content-Disposition` when the HTML page masks the displayed name.
