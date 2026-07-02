# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows Semantic
Versioning.

## [Unreleased]

### Added

- Bootstrap the Rust package, CLI shell, GPL compliance files, and CI workflow.
- Implement single-file and matomete downloads with sequential execution.
- Add download keys via `--key`, `--password`, and `-k`.
- Add resumable `.part` downloads with `--no-resume`.
- Add final `--json` output and CLI snapshot coverage.
- Add parser fixtures for matomete, password-required, wrong-key, missing, expired, and blocked pages.
- Preserve real UTF-8 filenames from `Content-Disposition` when the HTML page masks the displayed name.
