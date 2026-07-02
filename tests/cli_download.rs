// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";

#[tokio::test]
async fn cli_download_success_writes_file() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    mount_file(&server, binary_body(1024), Some(1024)).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("gfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("example file.bin"));

    assert_eq!(
        std::fs::read(temp.path().join("example file.bin"))
            .unwrap()
            .len(),
        1024
    );
}

#[test]
fn cli_download_invalid_url_exits_10() {
    Command::cargo_bin("gfile")
        .unwrap()
        .args(["download", "http://23.gigafile.nu/0123abcd-000000example"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains(
            "not a supported GigaFile download URL",
        ));
}

#[tokio::test]
async fn cli_download_parse_error_exits_13() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_broken.html")).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("gfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(temp.path())
        .assert()
        .code(13)
        .stderr(predicate::str::contains("missing #dl"));
}

#[test]
fn cli_download_size_mismatch_exits_17() {
    let server_uri = start_raw_mismatch_server(
        include_str!("fixtures/single_basic.html")
            .as_bytes()
            .to_vec(),
        binary_body(1024),
        2048,
    );
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("gfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["download", &format!("{server_uri}/{FILE_ID}"), "-o"])
        .arg(temp.path())
        .assert()
        .code(17)
        .stderr(predicate::str::contains("downloaded size did not match"));
}

#[tokio::test]
async fn cli_download_existing_target_without_force_exits_18() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("example file.bin"), b"existing").unwrap();

    Command::cargo_bin("gfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(temp.path())
        .assert()
        .code(18)
        .stderr(predicate::str::contains("target exists"));
}

#[tokio::test]
async fn cli_download_key_required_page_without_key_exits_15() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/page_needs_key.html")).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("gfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(temp.path())
        .assert()
        .code(15)
        .stderr(predicate::str::contains("requires a download key"));
}

#[tokio::test]
async fn cli_download_notfound_page_exits_14() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/page_notfound.html")).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("gfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(temp.path())
        .assert()
        .code(14)
        .stderr(predicate::str::contains("not found or has expired"));
}

#[tokio::test]
async fn cli_download_matomete_output_file_exits_2() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/matomete_two_files.html")).await;
    let temp = TempDir::new().unwrap();
    let output_file = temp.path().join("bundle.bin");

    Command::cargo_bin("gfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(output_file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "matomete downloads require --output",
        ));
}

async fn mount_page(server: &MockServer, body: &'static str) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/html"))
        .mount(server)
        .await;
}

async fn mount_file(server: &MockServer, body: Vec<u8>, content_length: Option<usize>) {
    let mut response = ResponseTemplate::new(200).set_body_bytes(body);
    if let Some(content_length) = content_length {
        response = response.insert_header("Content-Length", content_length.to_string());
    }
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(response)
        .mount(server)
        .await;
}

fn binary_body(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
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
