// SPDX-License-Identifier: MIT

use std::{
    fs,
    io::{Cursor, Write},
    path::PathBuf,
    process::Command,
};

use assert_cmd::prelude::*;
use flate2::{Compression, write::GzEncoder};
use predicates::prelude::*;
use rgfile::self_update::{archive_name, current_release_target};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
#[tokio::test]
async fn self_update_replaces_temp_copy_and_leaves_it_executable() {
    let Some(target) = current_release_target() else {
        return;
    };
    let server = MockServer::start().await;
    let tag = "v9.9.9";
    let version = "9.9.9";
    let asset = archive_name(version, target);
    let source = cargo_bin_path();
    let mut updated_binary = fs::read(&source).unwrap();
    updated_binary.extend_from_slice(b"\nRGFILE-SELF-UPDATE-TEST\n");
    let archive = release_archive(version, target, &updated_binary);

    mount_latest_release(&server, tag).await;
    mount_asset(&server, tag, &asset, archive.clone()).await;
    mount_asset(&server, tag, "SHA256SUMS", checksum_body(&asset, &archive)).await;

    let temp = TempDir::new().unwrap();
    let copy = temp.path().join("rgfile-copy");
    fs::copy(source, &copy).unwrap();
    let mut permissions = fs::metadata(&copy).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&copy, permissions).unwrap();

    Command::new(&copy)
        .env("RGFILE_TEST_UPDATE_BASE_URL", server.uri())
        .env("RGFILE_TEST_FORCE_UPDATE", "1")
        .args(["--no-config", "self-update"])
        .assert()
        .success()
        .stdout(predicate::str::contains("updated rgfile"));

    let replaced = fs::read(&copy).unwrap();
    assert!(replaced.ends_with(b"\nRGFILE-SELF-UPDATE-TEST\n"));
    let mode = fs::metadata(&copy).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0);

    Command::new(&copy).arg("--version").assert().success();
}

#[tokio::test]
async fn self_update_already_up_to_date_does_not_download_assets() {
    let Some(_target) = current_release_target() else {
        return;
    };
    let server = MockServer::start().await;
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    mount_latest_release(&server, &tag).await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("RGFILE_TEST_UPDATE_BASE_URL", server.uri())
        .args(["--no-config", "self-update"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already up to date"));
}

#[tokio::test]
async fn self_update_checksum_mismatch_refuses_install() {
    let Some(target) = current_release_target() else {
        return;
    };
    let server = MockServer::start().await;
    let tag = "v9.9.8";
    let version = "9.9.8";
    let asset = archive_name(version, target);
    let archive = release_archive(version, target, b"not a real executable");
    let wrong_hash = "0".repeat(64);
    let checksums = format!("{wrong_hash}  {asset}\n").into_bytes();

    mount_latest_release(&server, tag).await;
    mount_asset(&server, tag, &asset, archive).await;
    mount_asset(&server, tag, "SHA256SUMS", checksums).await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("RGFILE_TEST_UPDATE_BASE_URL", server.uri())
        .env("RGFILE_TEST_FORCE_UPDATE", "1")
        .args(["--no-config", "self-update"])
        .assert()
        .code(20)
        .stderr(predicate::str::contains("Checksum verification failed"))
        .stderr(predicate::str::contains("expected 000000"));
}

#[tokio::test]
async fn self_update_missing_target_in_checksums_is_parse_error() {
    let Some(target) = current_release_target() else {
        return;
    };
    let server = MockServer::start().await;
    let tag = "v9.9.7";
    let version = "9.9.7";
    let asset = archive_name(version, target);
    let archive = release_archive(version, target, b"not a real executable");

    mount_latest_release(&server, tag).await;
    mount_asset(&server, tag, &asset, archive).await;
    mount_asset(
        &server,
        tag,
        "SHA256SUMS",
        b"abc123  rgfile-9.9.7-other-target.tar.gz\n".to_vec(),
    )
    .await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("RGFILE_TEST_UPDATE_BASE_URL", server.uri())
        .env("RGFILE_TEST_FORCE_UPDATE", "1")
        .args(["--no-config", "self-update"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("SHA256SUMS has no entry"));
}

async fn mount_latest_release(server: &MockServer, tag: &str) {
    Mock::given(method("HEAD"))
        .and(path("/releases/latest"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/releases/tag/{tag}", server.uri())),
        )
        .mount(server)
        .await;
    Mock::given(path(format!("/releases/tag/{tag}")))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

async fn mount_asset(server: &MockServer, tag: &str, name: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(format!("/releases/download/{tag}/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}

fn cargo_bin_path() -> PathBuf {
    let command = Command::cargo_bin("rgfile").unwrap();
    command.get_program().to_owned().into()
}

fn checksum_body(asset: &str, archive: &[u8]) -> Vec<u8> {
    format!("{}  {asset}\n", sha256_hex(archive)).into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn release_archive(version: &str, target: &str, binary: &[u8]) -> Vec<u8> {
    let name = if target.ends_with("windows-msvc") {
        format!("rgfile-{version}-{target}/rgfile.exe")
    } else {
        format!("rgfile-{version}-{target}/rgfile")
    };

    if target.ends_with("windows-msvc") {
        zip_archive(&name, binary)
    } else {
        tar_gz_archive(&name, binary)
    }
}

fn tar_gz_archive(name: &str, binary: &[u8]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    archive
        .append_data(&mut header, name, Cursor::new(binary))
        .unwrap();
    let encoder = archive.into_inner().unwrap();
    encoder.finish().unwrap()
}

fn zip_archive(name: &str, binary: &[u8]) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    archive.start_file(name, options).unwrap();
    archive.write_all(binary).unwrap();
    archive.finish().unwrap().into_inner()
}
