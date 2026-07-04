// SPDX-License-Identifier: MIT

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";

#[tokio::test]
async fn cli_download_success_writes_file() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    mount_file(&server, binary_body(1024), Some(1024)).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
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
    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "download",
            "http://23.gigafile.nu/0123abcd-000000example",
        ])
        .assert()
        .code(10)
        .stderr(predicate::str::contains(
            "not a supported GigaFile download URL",
        ));
}

#[test]
fn cli_download_dl_alias_matches_download() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "dl",
            "http://23.gigafile.nu/0123abcd-000000example",
        ])
        .assert()
        .code(10)
        .stderr(predicate::str::contains(
            "not a supported GigaFile download URL",
        ));
}

#[test]
fn cli_download_threads_zero_exits_2() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "download",
            "--threads",
            "0",
            "https://23.gigafile.nu/0123abcd-000000example",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("download threads must be between"));
}

#[test]
fn cli_download_threads_seventeen_exits_2() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "download",
            "--threads",
            "17",
            "https://23.gigafile.nu/0123abcd-000000example",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("download threads must be between"));
}

#[tokio::test]
async fn cli_download_parse_error_exits_13() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_broken.html")).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
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

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{server_uri}/{FILE_ID}"),
            "-o",
        ])
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

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(temp.path())
        .assert()
        .code(18)
        .stderr(predicate::str::contains("already exists"))
        .stderr(predicate::str::contains("--force"));
}

#[tokio::test]
async fn cli_download_key_required_page_without_key_exits_15() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/page_needs_key.html")).await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
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

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
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

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(output_file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "matomete downloads require --output",
        ));
}

#[tokio::test]
async fn cli_download_threads_do_not_log_dlkey() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(4096);
    let keyed_requests = Arc::new(AtomicUsize::new(0));
    let responder_keyed = Arc::clone(&keyed_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if request
                .url
                .query_pairs()
                .any(|(key, value)| key == "dlkey" && value == "EXAMPLE-KEY-0000")
            {
                responder_keyed.fetch_add(1, Ordering::SeqCst);
            }
            if let Some((start, end)) = range_header(request) {
                range_response(&responder_body, start, end)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", responder_body.len().to_string())
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(responder_body.clone())
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "-vv",
            "download",
            "--threads",
            "4",
            "--key",
            "EXAMPLE-KEY-0000",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("EXAMPLE-KEY-0000").not())
        .stderr(predicate::str::contains("dlkey=EXAMPLE-KEY-0000").not());

    assert_eq!(keyed_requests.load(Ordering::SeqCst), 4);
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

fn range_header(request: &Request) -> Option<(u64, u64)> {
    let value = request.headers.get("range")?.to_str().ok()?;
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn range_response(body: &[u8], start: u64, end: u64) -> ResponseTemplate {
    ResponseTemplate::new(206)
        .insert_header(
            "Content-Range",
            format!("bytes {start}-{end}/{}", body.len()),
        )
        .insert_header("Content-Length", (end - start + 1).to_string())
        .insert_header("Content-Type", "application/octet-stream")
        .set_body_bytes(body[start as usize..=end as usize].to_vec())
}

#[cfg(unix)]
#[test]
fn cli_download_sigint_prints_resume_summary() {
    let prefix = binary_body(512 * 1024);
    let declared_len = 2 * 1024 * 1024;
    let server_uri = start_stalling_download_server(
        include_str!("fixtures/single_basic.html")
            .as_bytes()
            .to_vec(),
        prefix,
        declared_len,
    );
    let temp = TempDir::new().unwrap();
    let part_path = temp.path().join("example file.bin.part");

    let child = Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "download",
            &format!("{server_uri}/{FILE_ID}"),
            "-o",
        ])
        .arg(temp.path())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let part_has_data = |path: &std::path::Path| {
        std::fs::metadata(path)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if part_has_data(&part_path) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        part_has_data(&part_path),
        "partial file never appeared before interrupt"
    );

    std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "stderr: {stderr}");
    assert!(stderr.contains("Interrupted at"), "stderr: {stderr}");
    assert!(
        stderr.contains("Partial download kept:"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("Re-run the same command to resume."),
        "stderr: {stderr}"
    );
    assert!(part_path.exists());
}

#[cfg(unix)]
fn start_stalling_download_server(
    page_body: Vec<u8>,
    file_prefix: Vec<u8>,
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
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.write_all(&file_prefix).unwrap();
                stream.flush().unwrap();
                // Keep the connection open so the client stays mid-transfer
                // until the test delivers SIGINT.
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        }
    });
    format!("http://{addr}")
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
