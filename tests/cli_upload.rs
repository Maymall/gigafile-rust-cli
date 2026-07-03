// SPDX-License-Identifier: GPL-3.0-only

use std::{process::Command, time::Duration};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";

#[tokio::test]
async fn cli_upload_success_prints_url() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload(&server, Some(format!("{}/{FILE_ID}", server.uri()))).await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "cli.bin", b"hello");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .args(["upload", "--no-verify"])
        .arg(file)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "{}/{FILE_ID}",
            server.uri()
        )));
}

#[tokio::test]
async fn cli_upload_missing_url_exits_19() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload(&server, None).await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "missing.bin", b"hello");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .args(["upload", "--no-verify"])
        .arg(file)
        .assert()
        .code(19)
        .stderr(predicate::str::contains("final upload response"));
}

#[tokio::test]
async fn cli_upload_verify_mismatch_exits_20() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload(&server, Some(format!("{}/{FILE_ID}", server.uri()))).await;
    mount_download_page(&server).await;
    Mock::given(method("HEAD"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "9"))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "mismatch.bin", b"hello");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .args(["upload", "--timeout", "1"])
        .arg(file)
        .assert()
        .code(20)
        .stderr(predicate::str::contains("Upload verification failed"));
}

#[test]
fn cli_upload_empty_file_exits_2() {
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "empty.bin", b"");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["upload", "--no-verify"])
        .arg(file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("must not be empty"));
}

#[test]
fn cli_upload_invalid_chunk_size_exits_2() {
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "chunk.bin", b"hello");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["upload", "--chunk-size", "512K", "--no-verify"])
        .arg(file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("chunk size must be between"));
}

async fn mount_landing(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"<script>var server = "{}";</script>"#, server.uri()),
            "text/html",
        ))
        .mount(server)
        .await;
}

async fn mount_upload(server: &MockServer, url: Option<String>) {
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |_request: &Request| {
            let body = if let Some(url) = &url {
                serde_json::json!({ "status": 0, "url": url })
            } else {
                serde_json::json!({ "status": 0 })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(server)
        .await;
}

async fn mount_download_page(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/single_basic.html"), "text/html")
                .set_delay(Duration::from_millis(1)),
        )
        .mount(server)
        .await;
}

fn write_file(temp: &TempDir, name: &str, body: &[u8]) -> std::path::PathBuf {
    let path = temp.path().join(name);
    std::fs::write(&path, body).unwrap();
    path
}
