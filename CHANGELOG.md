# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows Semantic
Versioning.

## [Unreleased]

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
