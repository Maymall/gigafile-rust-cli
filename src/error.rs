// SPDX-License-Identifier: MIT

use std::{
    error::Error,
    fmt,
    io::{self, ErrorKind},
    path::PathBuf,
};

use thiserror::Error;

pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum GfileError {
    #[error("usage error")]
    Usage { message: String },

    #[error("invalid GigaFile URL")]
    InvalidUrl { url: String },

    #[error("network error")]
    Network {
        #[source]
        source: BoxError,
        context: String,
    },

    #[error("unexpected HTTP status {status}")]
    HttpStatus { status: u16, url_redacted: String },

    #[error("page parse failed: {what}")]
    Parse { what: String, hint: String },

    #[error("file not found or expired")]
    NotFoundOrExpired,

    #[error("download key required")]
    KeyRequired,

    #[error("download key rejected")]
    KeyWrong,

    #[error("size mismatch: expected {expected} bytes, got {actual} bytes")]
    SizeMismatch { expected: u64, actual: u64 },

    #[error("I/O error during {op} at {}", path.display())]
    Io {
        #[source]
        source: io::Error,
        path: PathBuf,
        op: IoOp,
    },

    #[error("download target is locked at {}", path.display())]
    TargetLocked { path: PathBuf },

    #[error("download target already exists at {}", path.display())]
    TargetExists { path: PathBuf },

    #[error("delete rejected")]
    DeleteRejected { detail: String, status: Option<i64> },

    #[error("upload rejected")]
    UploadRejected {
        detail: String,
        status: Option<u16>,
        retryable: bool,
    },

    #[error("upload verification failed: expected {expected} bytes, got {actual} bytes")]
    VerifyFailed { expected: u64, actual: u64 },

    #[error("checksum verification failed")]
    ChecksumMismatch { expected: String, actual: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOp {
    Create,
    Write,
    Rename,
    Read,
    Metadata,
}

impl fmt::Display for IoOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = match self {
            IoOp::Create => "create",
            IoOp::Write => "write",
            IoOp::Rename => "rename",
            IoOp::Read => "read",
            IoOp::Metadata => "metadata",
        };
        f.write_str(op)
    }
}

impl GfileError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage { .. } => 2,
            Self::InvalidUrl { .. } => 10,
            Self::Network { .. } => 11,
            Self::HttpStatus { .. } => 12,
            Self::Parse { .. } => 13,
            Self::NotFoundOrExpired => 14,
            Self::KeyRequired => 15,
            Self::KeyWrong => 16,
            Self::SizeMismatch { .. } => 17,
            Self::Io { .. } => 18,
            Self::TargetExists { .. } => 18,
            Self::UploadRejected { .. } => 19,
            Self::VerifyFailed { .. } => 20,
            Self::ChecksumMismatch { .. } => 20,
            Self::TargetLocked { .. } => 21,
            Self::DeleteRejected { .. } => 22,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage { .. } => "usage",
            Self::InvalidUrl { .. } => "invalid_url",
            Self::Network { .. } => "network",
            Self::HttpStatus { .. } => "http_status",
            Self::Parse { .. } => "parse",
            Self::NotFoundOrExpired => "not_found_or_expired",
            Self::KeyRequired => "key_required",
            Self::KeyWrong => "password_wrong",
            Self::SizeMismatch { .. } => "size_mismatch",
            Self::Io { .. } => "io",
            Self::TargetLocked { .. } => "target_locked",
            Self::TargetExists { .. } => "target_exists",
            Self::DeleteRejected { .. } => "delete_rejected",
            Self::UploadRejected { .. } => "upload_rejected",
            Self::VerifyFailed { .. } => "verify_failed",
            Self::ChecksumMismatch { .. } => "verify_failed",
        }
    }

    pub fn user_message(&self) -> String {
        match self {
            Self::Usage { message } => sanitize_message(message),
            Self::InvalidUrl { .. } => concat!(
                "The URL is not a supported GigaFile download URL. ",
                "Check that it is a public file page URL and try again."
            )
            .to_owned(),
            Self::Network { context, .. } => format!(
                "The network request failed while {}. Check your connection, proxy settings, and retry limit before trying again.",
                sanitize_message(context)
            ),
            Self::HttpStatus { status, .. } => format!(
                "The server returned unexpected HTTP status {status}. Try again later or rerun with -vv for diagnostics."
            ),
            Self::Parse { what, hint } => format!(
                "The page could not be parsed: {}. {}",
                sanitize_message(what),
                sanitize_message(hint)
            ),
            Self::NotFoundOrExpired => concat!(
                "The file was not found or has expired. ",
                "Confirm that the link is correct and that the file is still available."
            )
            .to_owned(),
            Self::KeyRequired => concat!(
                "This file requires a download key. ",
                "Provide the key with --key/--password or use the interactive prompt when available."
            )
            .to_owned(),
            Self::KeyWrong => concat!(
                "The download key was rejected. ",
                "Check the key and remember that it is case-sensitive."
            )
            .to_owned(),
            Self::SizeMismatch { expected, actual } => format!(
                "The downloaded size did not match the server header: expected {expected} bytes, got {actual} bytes. Keep the .part file for diagnostics or retry the download."
            ),
            Self::Io { source, path, op } => io_message(source, path, *op),
            Self::TargetLocked { path } => format!(
                "Another rgfile process appears to be downloading this file. Wait for it to finish, or remove the lock file if that process crashed: {}",
                path.display()
            ),
            Self::TargetExists { path } => format!(
                "The download target already exists: {}. Pass --force to overwrite it, or move the existing file away.",
                path.display()
            ),
            Self::DeleteRejected { detail, status } => {
                let status = status
                    .map(|status| format!(" (delete status {status})"))
                    .unwrap_or_default();
                format!(
                    "The delete request was rejected{status}: {}. Check the delete key and the URL, then try again.",
                    sanitize_message(detail)
                )
            }
            Self::UploadRejected { detail, .. } => format!(
                "The upload was rejected: {}. Retry later; if it keeps failing, rerun with -vv and report the response details.",
                sanitize_message(detail)
            ),
            Self::VerifyFailed { expected, actual } => format!(
                "Upload verification failed: expected {expected} bytes, got {actual} bytes. Re-upload the file before sharing the link."
            ),
            Self::ChecksumMismatch { expected, actual } => format!(
                "Checksum verification failed: expected {expected}, got {actual}. The downloaded update was not installed."
            ),
        }
    }
}

fn io_message(source: &io::Error, path: &std::path::Path, op: IoOp) -> String {
    if source.kind() == ErrorKind::PermissionDenied {
        if let Some(hint) = install_hint(path) {
            return format!(
                "Permission was denied while trying to {op} {}. {hint}",
                path.display()
            );
        }
        return format!(
            "Permission was denied while trying to {op} {}. Check the directory permissions and choose a writable destination.",
            path.display()
        );
    }

    if matches!(source.raw_os_error(), Some(28) | Some(112)) {
        return format!(
            "The disk appears to be full while trying to {op} {}. Free space or choose another destination and retry.",
            path.display()
        );
    }

    format!(
        "A local I/O error occurred while trying to {op} {}: {source}. Check the path and retry.",
        path.display()
    )
}

fn install_hint(path: &std::path::Path) -> Option<&'static str> {
    let text = path.to_string_lossy();
    if text.contains(".cargo/bin") {
        Some("This looks like a cargo-installed binary; run `cargo install rgfile` to upgrade it.")
    } else if text.contains("Cellar") || text.contains("linuxbrew") || text.contains("homebrew") {
        Some("This looks like a Homebrew-managed binary; run `brew upgrade rgfile` instead.")
    } else if text.starts_with("/usr/bin") || text.starts_with("/usr/local/bin") {
        Some(
            "This system directory is not writable; reinstall the .deb package or run the installer with appropriate privileges.",
        )
    } else {
        None
    }
}

fn sanitize_message(value: &str) -> String {
    let mut output = value.to_owned();
    redact_assignment(&mut output, "dlkey=", "redacted-download-key");
    redact_assignment(&mut output, "delkey=", "redacted-delete-key");
    redact_assignment(&mut output, "delete_key=", "redacted-delete-key");
    redact_json_string_field(&mut output, "delkey");
    redact_json_string_field(&mut output, "delete_key");
    output
}

fn redact_assignment(output: &mut String, marker: &str, replacement: &str) {
    while let Some(start) = output.find(marker) {
        let value_start = start + marker.len();
        let value_end = output[value_start..]
            .find(|ch: char| ch == '&' || ch.is_ascii_whitespace())
            .map(|offset| value_start + offset)
            .unwrap_or(output.len());
        output.replace_range(start..value_end, replacement);
    }
}

fn redact_json_string_field(output: &mut String, field: &str) {
    let marker = format!("\"{field}\"");
    let mut search_from = 0;
    while let Some(relative) = output[search_from..].find(&marker) {
        let field_start = search_from + relative;
        let Some(colon_relative) = output[field_start + marker.len()..].find(':') else {
            break;
        };
        let colon = field_start + marker.len() + colon_relative;
        let value_start = colon
            + 1
            + output[colon + 1..]
                .chars()
                .take_while(|ch| ch.is_ascii_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
        if !output[value_start..].starts_with('"') {
            search_from = value_start;
            continue;
        }
        let content_start = value_start + 1;
        let Some(content_end_relative) = output[content_start..].find('"') else {
            break;
        };
        let content_end = content_start + content_end_relative;
        output.replace_range(content_start..content_end, "***");
        search_from = content_start + 3;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_matches_spec() {
        for (error, expected) in error_cases() {
            assert_eq!(error.exit_code(), expected);
        }
    }

    #[test]
    fn user_message_is_nonempty_and_has_no_dlkey_parameter() {
        for (error, _) in error_cases() {
            let message = error.user_message();
            assert!(!message.trim().is_empty());
            assert!(!message.contains("dlkey="), "{message}");
        }
    }

    #[test]
    fn user_message_redacts_upload_delete_keys() {
        let error = GfileError::UploadRejected {
            detail: r#"response {"delkey":"EXAMPLE-DELKEY-0000","delete_key":"EXAMPLE-DELETE-0000"} delkey=EXAMPLE-DELKEY-0000"#.to_owned(),
            status: None,
            retryable: false,
        };

        let message = error.user_message();

        assert!(!message.contains("EXAMPLE-DELKEY-0000"), "{message}");
        assert!(!message.contains("EXAMPLE-DELETE-0000"), "{message}");
        assert!(message.contains(r#""delkey":"***""#), "{message}");
        assert!(message.contains("redacted-delete-key"), "{message}");
    }

    #[test]
    fn user_message_redacts_delete_rejected_detail() {
        let error = GfileError::DeleteRejected {
            detail: "delkey=EXAMPLE-DELKEY-0000".to_owned(),
            status: Some(1),
        };

        let message = error.user_message();

        assert!(message.contains("delete status 1"), "{message}");
        assert!(!message.contains("EXAMPLE-DELKEY-0000"), "{message}");
        assert!(message.contains("redacted-delete-key"), "{message}");
    }

    fn error_cases() -> Vec<(GfileError, u8)> {
        vec![
            (
                GfileError::Usage {
                    message: "output must be a directory for matomete pages".to_owned(),
                },
                2,
            ),
            (
                GfileError::InvalidUrl {
                    url: "https://23.gigafile.nu/0123abcd-000000example?dlkey=EXAMPLE-KEY-0000"
                        .to_owned(),
                },
                10,
            ),
            (
                GfileError::Network {
                    source: Box::new(io::Error::new(ErrorKind::TimedOut, "timeout")),
                    context: "GET download.php?file=0123abcd-000000example&dlkey=EXAMPLE-KEY-0000"
                        .to_owned(),
                },
                11,
            ),
            (
                GfileError::HttpStatus {
                    status: 503,
                    url_redacted: "download.php?file=0123abcd-000000example&dlkey=EXAMPLE-KEY-0000"
                        .to_owned(),
                },
                12,
            ),
            (
                GfileError::Parse {
                    what: "missing #dl".to_owned(),
                    hint: "use --dump-page".to_owned(),
                },
                13,
            ),
            (GfileError::NotFoundOrExpired, 14),
            (GfileError::KeyRequired, 15),
            (GfileError::KeyWrong, 16),
            (
                GfileError::SizeMismatch {
                    expected: 1024,
                    actual: 512,
                },
                17,
            ),
            (
                GfileError::Io {
                    source: io::Error::new(ErrorKind::PermissionDenied, "denied"),
                    path: PathBuf::from("example file.bin"),
                    op: IoOp::Read,
                },
                18,
            ),
            (
                GfileError::TargetLocked {
                    path: PathBuf::from("example file.bin.part.json.lock"),
                },
                21,
            ),
            (
                GfileError::TargetExists {
                    path: PathBuf::from("example file.bin"),
                },
                18,
            ),
            (
                GfileError::DeleteRejected {
                    detail: "delete status 1".to_owned(),
                    status: Some(1),
                },
                22,
            ),
            (
                GfileError::UploadRejected {
                    detail: "not implemented".to_owned(),
                    status: None,
                    retryable: false,
                },
                19,
            ),
            (
                GfileError::VerifyFailed {
                    expected: 1024,
                    actual: 512,
                },
                20,
            ),
            (
                GfileError::ChecksumMismatch {
                    expected: "expected-hash".to_owned(),
                    actual: "actual-hash".to_owned(),
                },
                20,
            ),
        ]
    }
}
