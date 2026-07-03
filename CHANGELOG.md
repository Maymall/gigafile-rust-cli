# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows Semantic
Versioning.

## [Unreleased]

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
