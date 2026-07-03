// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
};

use assert_cmd::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";

#[test]
fn snapshot_rgfile_help() {
    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    insta::assert_snapshot!(normalize_help(&output.stdout));
}

#[test]
fn snapshot_download_help() {
    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .args(["download", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = normalize_help(&output.stdout);
    assert!(!stdout.contains("GFILE_TEST_ALLOW_ANY_HOST"));
    assert!(!stdout.contains("GFILE_TEST_ENTRY_URL"));
    insta::assert_snapshot!(stdout);
}

#[test]
fn snapshot_upload_help() {
    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .args(["upload", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    insta::assert_snapshot!(normalize_help(&output.stdout));
}

#[tokio::test]
async fn snapshot_json_single_success() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    mount_file(&server, FILE_ID, b"hello".to_vec(), 5, 200).await;
    let temp = TempDir::new().unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "download",
            "--json",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(temp.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    insta::assert_snapshot!(normalize_json(&output.stdout));
}

#[tokio::test]
async fn snapshot_json_matomete_partial_failure() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/matomete_two_files.html")).await;
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "5")
                .insert_header("Content-Type", "application/octet-stream")
                .insert_header(
                    "Content-Disposition",
                    "attachment; filename*=UTF-8''example%20file.bin",
                )
                .set_body_bytes(b"first".to_vec()),
        )
        .mount(&server)
        .await;
    mount_file(&server, "0123abcd-000000example-2", Vec::new(), 0, 503).await;
    let temp = TempDir::new().unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "download",
            "--json",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(temp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(12), "{output:?}");
    insta::assert_snapshot!(normalize_json(&output.stdout));
}

#[tokio::test]
async fn snapshot_json_key_wrong_failure() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/page_wrong_key.html"), "text/html"),
        )
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "download",
            "--json",
            "--key",
            "EXAMPLE-KEY-0000",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(temp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(16), "{output:?}");
    insta::assert_snapshot!(normalize_json(&output.stdout));
}

#[test]
fn snapshot_json_size_mismatch_failure() {
    let server_uri = start_raw_mismatch_server(
        include_str!("fixtures/single_basic.html")
            .as_bytes()
            .to_vec(),
        b"short".to_vec(),
        10,
    );
    let temp = TempDir::new().unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "download",
            "--json",
            &format!("{server_uri}/{FILE_ID}"),
            "-o",
        ])
        .arg(temp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(17), "{output:?}");
    insta::assert_snapshot!(normalize_json(&output.stdout));
}

#[tokio::test]
async fn snapshot_json_parse_failure() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_broken.html")).await;
    let temp = TempDir::new().unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "download",
            "--json",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(temp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(13), "{output:?}");
    insta::assert_snapshot!(normalize_json(&output.stdout));
}

#[tokio::test]
async fn snapshot_json_upload_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"<script>var server = "{}";</script>"#, server.uri()),
            "text/html",
        ))
        .mount(&server)
        .await;
    let upload_url = format!("{}/{FILE_ID}", server.uri());
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |_request: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": 0,
                "url": upload_url,
            }))
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("upload.bin");
    std::fs::write(&file, b"hello").unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .args(["upload", "--json", "--no-verify"])
        .arg(file)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    insta::assert_snapshot!(normalize_json(&output.stdout));
}

async fn mount_page(server: &MockServer, body: &'static str) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/html"))
        .mount(server)
        .await;
}

async fn mount_file(
    server: &MockServer,
    file_id: &'static str,
    body: Vec<u8>,
    len: usize,
    status: u16,
) {
    let response = ResponseTemplate::new(status)
        .insert_header("Content-Length", len.to_string())
        .insert_header("Content-Type", "application/octet-stream")
        .set_body_bytes(body);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", file_id))
        .respond_with(move |_request: &Request| response.clone())
        .mount(server)
        .await;
}

fn normalize_json(bytes: &[u8]) -> String {
    let mut value: Value = serde_json::from_slice(bytes).unwrap();
    redact_paths(&mut value);
    serde_json::to_string_pretty(&value).unwrap()
}

fn normalize_help(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace("rgfile.exe", "rgfile")
}

fn redact_paths(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("path") {
                map.insert("path".to_owned(), Value::String("<PATH>".to_owned()));
            }
            if map.contains_key("url") {
                map.insert("url".to_owned(), Value::String("<URL>".to_owned()));
            }
            for value in map.values_mut() {
                redact_paths(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_paths(value);
            }
        }
        _ => {}
    }
}

fn start_raw_mismatch_server(
    page_body: Vec<u8>,
    file_body: Vec<u8>,
    declared_len: usize,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            if request.starts_with(&format!("GET /{FILE_ID} ")) {
                write_response(&mut stream, "text/html", page_body.len(), &page_body);
            } else {
                write_response(
                    &mut stream,
                    "application/octet-stream",
                    declared_len,
                    &file_body,
                );
            }
        }
    });
    format!("http://{addr}")
}

fn write_response(stream: &mut std::net::TcpStream, content_type: &str, len: usize, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}
