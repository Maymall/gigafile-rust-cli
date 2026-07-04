// SPDX-License-Identifier: MIT

use std::{
    env, fs, io,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use flate2::read::GzDecoder;
use reqwest::Url;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    error::{GfileError, IoOp, boxed},
    http,
};

const DEFAULT_UPDATE_BASE_URL: &str = "https://github.com/Maymall/gigafile-rust-cli";
const RETRIES: u32 = 3;

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
    if latest == current && !options.force {
        return Ok(SelfUpdateReport::AlreadyUpToDate { version: current });
    }

    let asset = archive_name(&latest, target);
    let workdir = WorkDir::create()?;
    let archive = download_release_file(&client, base_url, &tag, &asset).await?;
    let checksums = download_release_file(&client, base_url, &tag, "SHA256SUMS").await?;
    fs::write(workdir.path.join(&asset), &archive).map_err(|source| GfileError::Io {
        source,
        path: workdir.path.join(&asset),
        op: IoOp::Write,
    })?;
    fs::write(workdir.path.join("SHA256SUMS"), &checksums).map_err(|source| GfileError::Io {
        source,
        path: workdir.path.join("SHA256SUMS"),
        op: IoOp::Write,
    })?;
    verify_checksum(&checksums, &asset, &archive)?;
    let binary = extract_binary(&archive, &latest, target)?;
    let path = install_binary(&binary)?;

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
    let response = client
        .head(&url)
        .send()
        .await
        .map_err(|source| GfileError::Network {
            source: boxed(source),
            context: "resolving latest release".to_owned(),
        })?;
    if !response.status().is_success() {
        return Err(http::status_error(response.status(), &url));
    }
    tag_from_release_url(response.url())
}

fn tag_from_release_url(url: &Url) -> Result<String, GfileError> {
    let Some(tag) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
    else {
        return Err(latest_tag_parse_error(url));
    };
    if tag.starts_with('v')
        && tag[1..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit())
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
    let response =
        http::get_with_retries(client, &url, RETRIES, "downloading release asset").await?;
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|source| GfileError::Network {
            source: boxed(source),
            context: "reading release asset".to_owned(),
        })
}

fn verify_checksum(checksums: &[u8], asset: &str, archive: &[u8]) -> Result<(), GfileError> {
    let text = String::from_utf8_lossy(checksums);
    let expected = text
        .lines()
        .filter_map(parse_checksum_line)
        .find_map(|(hash, name)| (name == asset).then(|| hash.to_owned()))
        .ok_or_else(|| GfileError::Parse {
            what: format!("SHA256SUMS has no entry for {asset}"),
            hint: "The release assets are inconsistent; retry later.".to_owned(),
        })?;
    let actual = sha256_hex(archive);
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(GfileError::ChecksumMismatch { expected, actual })
    }
}

fn parse_checksum_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    let name = parts.next()?;
    Some((hash, name.trim_start_matches('*')))
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
    for entry in archive.entries().map_err(read_archive_error)? {
        let mut entry = entry.map_err(read_archive_error)?;
        if entry.path().map_err(read_archive_error)?.to_string_lossy() == expected {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(read_archive_error)?;
            return Ok(bytes);
        }
    }
    Err(archive_layout_error(&expected))
}

fn extract_zip_binary(archive: &[u8], version: &str, target: &str) -> Result<Vec<u8>, GfileError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(archive)).map_err(read_archive_error)?;
    let expected = format!("rgfile-{version}-{target}/rgfile.exe");
    let mut file = archive.by_name(&expected).map_err(read_archive_error)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(read_archive_error)?;
    Ok(bytes)
}

fn archive_layout_error(expected: &str) -> GfileError {
    GfileError::Parse {
        what: format!("release archive did not contain {expected}"),
        hint: "The release archive layout is unexpected; retry later.".to_owned(),
    }
}

fn install_binary(bytes: &[u8]) -> Result<PathBuf, GfileError> {
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
    install_binary_at(bytes, &target)
}

pub fn install_binary_at(bytes: &[u8], target: &Path) -> Result<PathBuf, GfileError> {
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
    fs::write(&temp_path, bytes).map_err(|source| GfileError::Io {
        source,
        path: temp_path.clone(),
        op: IoOp::Write,
    })?;
    set_executable(&temp_path)?;
    replace_binary(&temp_path, target)?;
    Ok(target.to_owned())
}

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
    let old_path = target.with_extension("old.exe");
    if old_path.exists() {
        fs::remove_file(&old_path).map_err(|source| GfileError::Io {
            source,
            path: old_path.clone(),
            op: IoOp::Write,
        })?;
    }
    fs::rename(target, &old_path).map_err(|source| GfileError::Io {
        source,
        path: target.to_owned(),
        op: IoOp::Rename,
    })?;
    if let Err(source) = fs::rename(temp_path, target) {
        let _ = fs::rename(&old_path, target);
        return Err(GfileError::Io {
            source,
            path: target.to_owned(),
            op: IoOp::Rename,
        });
    }
    let _ = fs::remove_file(&old_path);
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

struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn create() -> Result<Self, GfileError> {
        let path = env::temp_dir().join(format!("rgfile-update-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&path).map_err(|source| GfileError::Io {
            source,
            path: path.clone(),
            op: IoOp::Create,
        })?;
        Ok(Self { path })
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_line_parses_common_formats() {
        assert_eq!(
            parse_checksum_line("abc123  rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz"),
            Some(("abc123", "rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz"))
        );
        assert_eq!(
            parse_checksum_line("abc123 *rgfile-1.0.0-x86_64-pc-windows-msvc.zip"),
            Some(("abc123", "rgfile-1.0.0-x86_64-pc-windows-msvc.zip"))
        );
    }

    #[test]
    fn checksum_mismatch_reports_hashes() {
        let error = verify_checksum(
            b"000000  rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz\n",
            "rgfile-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
            b"archive",
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), 20);
        assert!(error.user_message().contains("expected 000000"));
    }
}
