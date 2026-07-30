// SPDX-License-Identifier: MIT

use std::{
    cmp::Ordering,
    env, fs, io,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use flate2::read::GzDecoder;
use reqwest::Url;
use semver::Version;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    error::{GfileError, IoOp, boxed},
    http,
};

const DEFAULT_UPDATE_BASE_URL: &str = "https://github.com/Maymall/gigafile-rust-cli";
const RETRIES: u32 = 3;
const UPDATE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
// Release binaries are currently single-digit MiB, while integration tests
// update an unstripped debug executable. This still stays below the archive
// cap and prevents two independently unbounded allocations.
const UPDATE_BINARY_LIMIT: u64 = 192 * 1024 * 1024;
const UPDATE_SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const UPDATE_SMOKE_OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);
const UPDATE_SMOKE_OUTPUT_LIMIT: usize = 16 * 1024;
const UPDATE_SMOKE_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct SelfUpdateOptions {
    pub base_url: Option<String>,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfUpdateReport {
    AlreadyUpToDate {
        version: String,
    },
    Updated {
        old_version: String,
        new_version: String,
        target: String,
        path: PathBuf,
    },
}

pub async fn self_update(options: SelfUpdateOptions) -> Result<SelfUpdateReport, GfileError> {
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let target = current_release_target().ok_or_else(|| GfileError::Usage {
        message:
            "self-update is not available for this platform; download a release archive manually"
                .to_owned(),
    })?;
    let base_url = options
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_UPDATE_BASE_URL)
        .trim_end_matches('/');
    let client = http::build_client(None)?;
    let tag = latest_release_tag(&client, base_url).await?;
    let latest = tag.trim_start_matches('v').to_owned();
    let current_version = Version::parse(&current).map_err(|_| GfileError::Parse {
        what: format!("current package version is not valid SemVer: {current}"),
        hint: "This is a packaging error; please report it.".to_owned(),
    })?;
    let latest_version = Version::parse(&latest).map_err(|_| GfileError::Parse {
        what: format!("latest release tag is not valid SemVer: {tag}"),
        hint: "The GitHub release tag is malformed; retry later.".to_owned(),
    })?;
    if latest_version.cmp_precedence(&current_version) != Ordering::Greater && !options.force {
        return Ok(SelfUpdateReport::AlreadyUpToDate { version: current });
    }

    let asset = archive_name(&latest, target);
    let archive = download_release_file(&client, base_url, &tag, &asset).await?;
    let checksums = download_release_file(&client, base_url, &tag, "SHA256SUMS").await?;
    verify_checksum(&checksums, &asset, &archive)?;
    let binary = extract_binary(&archive, &latest, target)?;
    let path = install_binary(&binary, &latest)?;

    Ok(SelfUpdateReport::Updated {
        old_version: current,
        new_version: latest,
        target: target.to_owned(),
        path,
    })
}

pub async fn latest_release_tag(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<String, GfileError> {
    let url = format!("{}/releases/latest", base_url.trim_end_matches('/'));
    let mut attempt = 0;
    loop {
        let result = tokio::time::timeout(UPDATE_IDLE_TIMEOUT, client.head(&url).send())
            .await
            .map_err(|_| GfileError::Network {
                source: boxed(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "latest release lookup timed out",
                )),
                context: "resolving latest release".to_owned(),
            })
            .and_then(|result| {
                result.map_err(|source| GfileError::Network {
                    source: boxed(source),
                    context: "resolving latest release".to_owned(),
                })
            });
        match result {
            Ok(response) if http::is_retryable_status(response.status()) && attempt < RETRIES => {
                drop(response);
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Ok(response) if response.status().is_success() => {
                return tag_from_release_url(response.url());
            }
            Ok(response) => return Err(http::status_error(response.status(), &url)),
            Err(error) if http::is_retryable(&error) && attempt < RETRIES => {
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn tag_from_release_url(url: &Url) -> Result<String, GfileError> {
    let Some(tag) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
    else {
        return Err(latest_tag_parse_error(url));
    };
    if tag
        .strip_prefix('v')
        .is_some_and(|version| Version::parse(version).is_ok())
    {
        Ok(tag.to_owned())
    } else {
        Err(latest_tag_parse_error(url))
    }
}

fn latest_tag_parse_error(url: &Url) -> GfileError {
    GfileError::Parse {
        what: format!("could not determine latest release tag from {url}"),
        hint: "The GitHub release redirect did not end in a v* tag.".to_owned(),
    }
}

async fn download_release_file(
    client: &reqwest::Client,
    base_url: &str,
    tag: &str,
    filename: &str,
) -> Result<Vec<u8>, GfileError> {
    let url = format!(
        "{}/releases/download/{}/{}",
        base_url.trim_end_matches('/'),
        tag,
        filename
    );
    let limit = if filename == "SHA256SUMS" {
        http::API_BODY_LIMIT
    } else {
        http::UPDATE_ARCHIVE_LIMIT
    };
    let mut attempt = 0;
    loop {
        let response = http::get_with_retries_and_timeout(
            client,
            &url,
            RETRIES,
            "downloading release asset",
            Some(UPDATE_IDLE_TIMEOUT),
        )
        .await?;
        match http::read_body_limited(
            response,
            limit,
            UPDATE_IDLE_TIMEOUT,
            "reading release asset",
        )
        .await
        {
            Ok(body) => return Ok(body),
            Err(error) if http::is_retryable(&error) && attempt < RETRIES => {
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn verify_checksum(checksums: &[u8], asset: &str, archive: &[u8]) -> Result<(), GfileError> {
    let text = std::str::from_utf8(checksums).map_err(|_| GfileError::Parse {
        what: "SHA256SUMS is not valid UTF-8".to_owned(),
        hint: "The release assets are inconsistent; retry later.".to_owned(),
    })?;
    let expected = checksum_for_asset(text, asset).ok_or_else(|| GfileError::Parse {
        what: format!("SHA256SUMS does not contain exactly one valid entry for {asset}"),
        hint: "The release assets are inconsistent; retry later.".to_owned(),
    })?;
    let actual = sha256_hex(archive);
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(GfileError::ChecksumMismatch {
            expected: expected.to_owned(),
            actual,
        })
    }
}

fn parse_checksum_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    let name = parts.next()?.trim_start_matches('*');
    if parts.next().is_some()
        || hash.len() != 64
        || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        || name.is_empty()
    {
        return None;
    }
    Some((hash, name))
}

fn checksum_for_asset<'a>(text: &'a str, asset: &str) -> Option<&'a str> {
    let mut expected = None;
    for line in text.lines() {
        if let Some((hash, name)) = parse_checksum_line(line) {
            if name == asset {
                if expected.is_some() {
                    return None;
                }
                expected = Some(hash);
            }
        } else if line
            .split_whitespace()
            .nth(1)
            .is_some_and(|name| name.trim_start_matches('*') == asset)
        {
            // A malformed line for the requested asset must not be ignored in
            // favor of a different duplicate line.
            return None;
        }
    }
    expected
}

fn extract_binary(archive: &[u8], version: &str, target: &str) -> Result<Vec<u8>, GfileError> {
    if archive_extension(target) == "zip" {
        extract_zip_binary(archive, version, target)
    } else {
        extract_tar_binary(archive, version, target)
    }
}

fn extract_tar_binary(archive: &[u8], version: &str, target: &str) -> Result<Vec<u8>, GfileError> {
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let expected = format!("rgfile-{version}-{target}/rgfile");
    let mut extracted = None;
    for entry in archive.entries().map_err(read_archive_error)? {
        let mut entry = entry.map_err(read_archive_error)?;
        if entry.path().map_err(read_archive_error)?.to_string_lossy() == expected {
            if !entry.header().entry_type().is_file() || extracted.is_some() {
                return Err(archive_layout_error(&expected));
            }
            extracted = Some(read_update_binary(&mut entry)?);
        }
    }
    extracted.ok_or_else(|| archive_layout_error(&expected))
}

fn extract_zip_binary(archive: &[u8], version: &str, target: &str) -> Result<Vec<u8>, GfileError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(archive)).map_err(read_archive_error)?;
    let expected = format!("rgfile-{version}-{target}/rgfile.exe");
    let mut extracted = None;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(read_archive_error)?;
        if file.name() == expected {
            if !file.is_file() || extracted.is_some() {
                return Err(archive_layout_error(&expected));
            }
            extracted = Some(read_update_binary(&mut file)?);
        }
    }
    extracted.ok_or_else(|| archive_layout_error(&expected))
}

fn read_update_binary(reader: &mut impl Read) -> Result<Vec<u8>, GfileError> {
    let mut bytes = Vec::new();
    reader
        .take(UPDATE_BINARY_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(read_archive_error)?;
    if bytes.len() as u64 > UPDATE_BINARY_LIMIT {
        return Err(GfileError::ResponseTooLarge {
            context: "extracting the self-update binary".to_owned(),
            limit: UPDATE_BINARY_LIMIT,
        });
    }
    if bytes.is_empty() {
        return Err(archive_layout_error("non-empty update binary"));
    }
    Ok(bytes)
}

fn archive_layout_error(expected: &str) -> GfileError {
    GfileError::Parse {
        what: format!("release archive did not contain {expected}"),
        hint: "The release archive layout is unexpected; retry later.".to_owned(),
    }
}

fn install_binary(bytes: &[u8], expected_version: &str) -> Result<PathBuf, GfileError> {
    let current = env::current_exe().map_err(|source| GfileError::Io {
        source,
        path: PathBuf::from("current executable"),
        op: IoOp::Metadata,
    })?;
    let target = fs::canonicalize(&current).map_err(|source| GfileError::Io {
        source,
        path: current.clone(),
        op: IoOp::Metadata,
    })?;
    install_binary_at_version(bytes, &target, expected_version)
}

pub fn install_binary_at(bytes: &[u8], target: &Path) -> Result<PathBuf, GfileError> {
    install_binary_at_version(bytes, target, env!("CARGO_PKG_VERSION"))
}

fn install_binary_at_version(
    bytes: &[u8],
    target: &Path,
    expected_version: &str,
) -> Result<PathBuf, GfileError> {
    let parent = target.parent().ok_or_else(|| GfileError::Io {
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable has no parent directory",
        ),
        path: target.to_owned(),
        op: IoOp::Write,
    })?;
    let temp_name = format!(
        ".rgfile-update-{}{}",
        Uuid::new_v4().simple(),
        executable_suffix()
    );
    let temp_path = parent.join(temp_name);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| GfileError::Io {
                source,
                path: temp_path.clone(),
                op: IoOp::Create,
            })?;
        file.write_all(bytes).map_err(|source| GfileError::Io {
            source,
            path: temp_path.clone(),
            op: IoOp::Write,
        })?;
        file.sync_all().map_err(|source| GfileError::Io {
            source,
            path: temp_path.clone(),
            op: IoOp::Write,
        })?;
        set_executable(&temp_path)?;
        // chmod changes inode metadata after the first data flush on Unix.
        // Sync the still-open staging file again so both the contents and
        // executable mode are durable before the rename.
        file.sync_all().map_err(|source| GfileError::Io {
            source,
            path: temp_path.clone(),
            op: IoOp::Write,
        })?;
        drop(file);
        smoke_test_binary(&temp_path, expected_version)?;
        replace_binary(&temp_path, target)?;
        sync_parent_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result.map(|()| target.to_owned())
}

fn smoke_test_binary(path: &Path, expected_version: &str) -> Result<(), GfileError> {
    let mut command = Command::new(path);
    command
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_smoke_command(&mut command);

    let mut child = command.spawn().map_err(|source| GfileError::Io {
        source,
        path: path.to_owned(),
        op: IoOp::Read,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        smoke_test_error("the staged executable did not expose its version output")
    })?;
    let (output_sender, output_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = output_sender.send(read_smoke_output(stdout));
    });

    let deadline = Instant::now() + UPDATE_SMOKE_TEST_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(UPDATE_SMOKE_POLL_INTERVAL);
            }
            Ok(None) => {
                terminate_smoke_child(&mut child);
                return Err(smoke_test_error(format!(
                    "the staged executable exceeded the {}-second time limit",
                    UPDATE_SMOKE_TEST_TIMEOUT.as_secs()
                )));
            }
            Err(source) => {
                terminate_smoke_child(&mut child);
                return Err(GfileError::Io {
                    source,
                    path: path.to_owned(),
                    op: IoOp::Read,
                });
            }
        }
    };
    if !status.success() {
        return Err(smoke_test_error(format!(
            "the staged executable exited with {status}"
        )));
    }

    let (output, exceeded_limit) = match output_receiver.recv_timeout(UPDATE_SMOKE_OUTPUT_TIMEOUT) {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => {
            return Err(GfileError::Io {
                source,
                path: path.to_owned(),
                op: IoOp::Read,
            });
        }
        Err(_) => {
            terminate_smoke_process_group(child.id());
            return Err(smoke_test_error(
                "the staged executable did not close its version output",
            ));
        }
    };
    if exceeded_limit {
        return Err(smoke_test_error(format!(
            "the staged executable produced more than {UPDATE_SMOKE_OUTPUT_LIMIT} bytes of version output"
        )));
    }
    validate_smoke_output(&output, expected_version)
}

fn read_smoke_output(mut reader: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(UPDATE_SMOKE_OUTPUT_LIMIT.min(1024));
    let mut exceeded_limit = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = UPDATE_SMOKE_OUTPUT_LIMIT.saturating_sub(output.len());
        let retained = remaining.min(read);
        output.extend_from_slice(&buffer[..retained]);
        exceeded_limit |= retained < read;
    }
    Ok((output, exceeded_limit))
}

fn validate_smoke_output(output: &[u8], expected_version: &str) -> Result<(), GfileError> {
    let output = std::str::from_utf8(output)
        .map_err(|_| smoke_test_error("the staged executable returned non-UTF-8 version output"))?;
    let output = output.strip_suffix('\n').unwrap_or(output);
    let output = output.strip_suffix('\r').unwrap_or(output);
    let expected = format!("rgfile {expected_version}");
    if output == expected {
        Ok(())
    } else {
        Err(smoke_test_error(format!(
            "expected version output {expected:?}, got {output:?}"
        )))
    }
}

fn smoke_test_error(detail: impl Into<String>) -> GfileError {
    GfileError::Parse {
        what: format!("self-update smoke test failed: {}", detail.into()),
        hint: "The staged update was not installed; verify the release assets and retry."
            .to_owned(),
    }
}

#[cfg(unix)]
fn configure_smoke_command(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_smoke_command(_command: &mut Command) {}

fn terminate_smoke_child(child: &mut Child) {
    terminate_smoke_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn terminate_smoke_process_group(child_id: u32) {
    if let Ok(process_group) = libc::pid_t::try_from(child_id) {
        // SAFETY: the smoke-test child is placed in a process group whose ID
        // is its process ID. A negative PID addresses only that child group.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_smoke_process_group(_child_id: u32) {}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), GfileError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|source| GfileError::Io {
            source,
            path: path.to_owned(),
            op: IoOp::Metadata,
        })?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(|source| GfileError::Io {
        source,
        path: path.to_owned(),
        op: IoOp::Write,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), GfileError> {
    Ok(())
}

#[cfg(unix)]
fn replace_binary(temp_path: &Path, target: &Path) -> Result<(), GfileError> {
    fs::rename(temp_path, target).map_err(|source| GfileError::Io {
        source,
        path: target.to_owned(),
        op: IoOp::Rename,
    })
}

#[cfg(windows)]
fn replace_binary(temp_path: &Path, target: &Path) -> Result<(), GfileError> {
    // MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH), implemented by the shared
    // filesystem helper, either replaces the destination in one operation or
    // leaves the running executable in place. This avoids the crash window
    // created by first renaming the current executable out of the way.
    crate::fsutil::replace_file(temp_path, target).map_err(|source| GfileError::Io {
        source,
        path: target.to_owned(),
        op: IoOp::Rename,
    })
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), GfileError> {
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| GfileError::Io {
            source,
            path: parent.to_owned(),
            op: IoOp::Write,
        })
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), GfileError> {
    // The Windows replacement uses MOVEFILE_WRITE_THROUGH.
    Ok(())
}

pub fn current_release_target() -> Option<&'static str> {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

pub fn archive_name(version: &str, target: &str) -> String {
    format!("rgfile-{version}-{target}.{}", archive_extension(target))
}

fn archive_extension(target: &str) -> &'static str {
    if target.ends_with("windows-msvc") {
        "zip"
    } else {
        "tar.gz"
    }
}

fn executable_suffix() -> &'static str {
    if cfg!(windows) { ".exe" } else { "" }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_archive_error(source: impl std::error::Error + Send + Sync + 'static) -> GfileError {
    GfileError::Parse {
        what: format!("failed to read release archive: {source}"),
        hint: "The downloaded release archive may be corrupt; retry later.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn duplicate_tar_archive(path: &str) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};

        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for body in [b"first".as_slice(), b"second".as_slice()] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, path, Cursor::new(body))
                .unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap()
    }

    fn empty_zip_archive(path: &str) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        archive.start_file(path, options).unwrap();
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn checksum_line_parses_common_formats() {
        const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_checksum_line(&format!(
                "{HASH}  rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz"
            )),
            Some((HASH, "rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz"))
        );
        assert_eq!(
            parse_checksum_line(&format!("{HASH} *rgfile-1.0.0-x86_64-pc-windows-msvc.zip")),
            Some((HASH, "rgfile-1.0.0-x86_64-pc-windows-msvc.zip"))
        );
        assert!(parse_checksum_line("abc123  asset.tar.gz").is_none());
    }

    #[test]
    fn checksum_mismatch_reports_hashes() {
        let error = verify_checksum(
            b"0000000000000000000000000000000000000000000000000000000000000000  rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz\n",
            "rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
            b"archive",
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), 20);
        assert!(error.user_message().contains("expected 000000"));
    }

    #[test]
    fn release_versions_are_compared_without_allowing_downgrades() {
        let current = Version::parse("1.2.3").unwrap();
        assert_eq!(
            Version::parse("1.2.4").unwrap().cmp_precedence(&current),
            Ordering::Greater
        );
        assert_eq!(
            Version::parse("1.2.3-alpha.10")
                .unwrap()
                .cmp_precedence(&Version::parse("1.2.3-alpha.2").unwrap()),
            Ordering::Greater
        );
        assert_eq!(
            Version::parse("1.2.3+build.2")
                .unwrap()
                .cmp_precedence(&Version::parse("1.2.3+build.1").unwrap()),
            Ordering::Equal
        );
        assert!(Version::parse("01.2.3").is_err());
    }

    #[test]
    fn checksum_rejects_duplicate_or_malformed_matching_entries() {
        const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let asset = "rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        assert!(
            checksum_for_asset(&format!("{HASH}  {asset}\n{HASH}  {asset}\n"), asset).is_none()
        );
        assert!(checksum_for_asset(&format!("bad  {asset}\n"), asset).is_none());
        assert!(checksum_for_asset(&format!("{HASH}  {asset} extra\n"), asset).is_none());
    }

    #[test]
    fn extracted_update_binary_must_be_unique_and_nonempty() {
        let linux_target = "x86_64-unknown-linux-gnu";
        let linux_path = "rgfile-1.0.0-x86_64-unknown-linux-gnu/rgfile";
        assert!(
            extract_tar_binary(&duplicate_tar_archive(linux_path), "1.0.0", linux_target).is_err()
        );

        let windows_target = "x86_64-pc-windows-msvc";
        let windows_path = "rgfile-1.0.0-x86_64-pc-windows-msvc/rgfile.exe";
        assert!(
            extract_zip_binary(&empty_zip_archive(windows_path), "1.0.0", windows_target).is_err()
        );

        assert!(read_update_binary(&mut Cursor::new(Vec::<u8>::new())).is_err());
    }

    #[test]
    fn smoke_test_output_requires_exact_expected_version() {
        assert!(validate_smoke_output(b"rgfile 1.2.3\n", "1.2.3").is_ok());
        assert!(validate_smoke_output(b"rgfile 1.2.3\r\n", "1.2.3").is_ok());
        assert!(validate_smoke_output(b"rgfile 1.2.4\n", "1.2.3").is_err());
        assert!(validate_smoke_output(b"rgfile 1.2.3 extra\n", "1.2.3").is_err());
        assert!(validate_smoke_output(b"rgfile 1.2.3\n\n", "1.2.3").is_err());
        assert!(validate_smoke_output(b"\xff", "1.2.3").is_err());
    }

    #[test]
    fn smoke_test_output_reader_is_bounded_while_draining_input() {
        let input = vec![b'x'; UPDATE_SMOKE_OUTPUT_LIMIT + 8192];
        let (output, exceeded_limit) = read_smoke_output(Cursor::new(input)).unwrap();

        assert_eq!(output.len(), UPDATE_SMOKE_OUTPUT_LIMIT);
        assert!(exceeded_limit);
    }
}
