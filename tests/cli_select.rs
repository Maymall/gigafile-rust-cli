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
async fn cli_download_select_out_of_range_exits_2_and_hints_info() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/matomete_two_files.html")).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            "--select",
            "3",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(temp.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("out of range"))
        .stderr(predicate::str::contains("rgfile info"));
}

#[test]
fn cli_download_select_invalid_format_exits_2() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "download",
            "--select",
            "1,z",
            "https://example.invalid/0123abcd-000000example",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid --select value"))
        .stderr(predicate::str::contains("rgfile info"));
}

async fn mount_page(server: &MockServer, body: &'static str) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/html"))
        .mount(server)
        .await;
}
