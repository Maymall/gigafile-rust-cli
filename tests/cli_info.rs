// SPDX-License-Identifier: MIT

use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const FILE_ID: &str = "0123abcd-000000example";

#[tokio::test]
async fn cli_info_single_does_not_request_download_endpoint() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "info",
            &format!("{}/{FILE_ID}", server.uri()),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("kind\tsingle"))
        .stdout(predicate::str::contains("[1]"))
        .stdout(predicate::str::contains(
            "display_name (may be masked)\texample file.bin",
        ));

    let download_requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/download.php")
        .count();
    assert_eq!(download_requests, 0);
}

#[tokio::test]
async fn cli_info_needs_key_succeeds_without_key() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/page_needs_key.html")).await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "info",
            &format!("{}/{FILE_ID}", server.uri()),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("key_required\ttrue"));
}

#[tokio::test]
async fn cli_info_notfound_exits_14() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/page_notfound.html"), "text/html"),
        )
        .mount(&server)
        .await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "info",
            &format!("{}/{FILE_ID}", server.uri()),
        ])
        .assert()
        .code(14)
        .stderr(predicate::str::contains("not found or has expired"));
}

#[tokio::test]
async fn cli_info_dump_page_writes_html() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let temp = TempDir::new().unwrap();
    let dump = temp.path().join("info.html");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "info",
            &format!("{}/{FILE_ID}", server.uri()),
            "--dump-page",
        ])
        .arg(&dump)
        .assert()
        .success()
        .stderr(predicate::str::contains("dumped page may contain"));

    let dumped = std::fs::read_to_string(dump).unwrap();
    assert!(dumped.contains("example file.bin"));
}

async fn mount_page(server: &MockServer, body: &'static str) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/html"))
        .mount(server)
        .await;
}
