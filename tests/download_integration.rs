// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rgfile::{
    download::{DownloadOptions, DownloadReport, download},
    error::GfileError,
};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";

#[tokio::test]
async fn download_single_success_writes_final_and_cleans_part_files() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(10 * 1024);
    mount_file(&server, 200, body.clone(), Some(body.len()), None).await;
    let temp = TempDir::new().unwrap();

    let report = download(options(&server, &temp, 3)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(outcome.bytes, Some(body.len() as u64));
    assert_eq!(std::fs::read(outcome.path.as_ref().unwrap()).unwrap(), body);
    assert!(
        !outcome
            .path
            .as_ref()
            .unwrap()
            .with_file_name("example file.bin.part")
            .exists()
    );
    assert!(
        !outcome
            .path
            .as_ref()
            .unwrap()
            .with_file_name("example file.bin.part.json")
            .exists()
    );
}

#[tokio::test]
async fn download_password_file_requires_key_and_sends_dlkey() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(512);
    let success_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if request
                .url
                .query_pairs()
                .any(|(key, value)| key == "dlkey" && value == "EXAMPLE-KEY-0000")
            {
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", success_body.len().to_string())
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(success_body.clone())
            } else {
                ResponseTemplate::new(200)
                    .set_body_raw(include_str!("fixtures/page_needs_key.html"), "text/html")
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();

    let error = download(options(&server, &temp, 0))
        .await
        .expect_err("missing key should fail");
    assert!(matches!(error, GfileError::KeyRequired));

    let mut opts = options(&server, &temp, 0);
    opts.key = Some("EXAMPLE-KEY-0000".to_owned());
    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(outcome.bytes, Some(body.len() as u64));
    assert_eq!(std::fs::read(outcome.path.as_ref().unwrap()).unwrap(), body);
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().any(|request| {
        request.url.path() == "/download.php"
            && request
                .url
                .query_pairs()
                .any(|(key, value)| key == "dlkey" && value == "EXAMPLE-KEY-0000")
    }));
}

#[tokio::test]
async fn download_single_japanese_name_preserves_filename_bytes() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_japanese.html")).await;
    let body = binary_body(1024);
    mount_file(&server, 200, body, Some(1024), None).await;
    let temp = TempDir::new().unwrap();

    let report = download(options(&server, &temp, 3)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(
        outcome
            .path
            .as_ref()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
        "テスト資料_2026.zip".as_bytes()
    );
}

#[tokio::test]
async fn download_content_disposition_overrides_masked_page_name() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_masked.html")).await;
    let body = binary_body(4096);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .insert_header("Content-Type", "application/octet-stream")
                .insert_header(
                    "Content-Disposition",
                    "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E3%83%86%E3%82%B9%E3%83%88%E8%B3%87%E6%96%99_2026.bin",
                )
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();

    let report = download(options(&server, &temp, 3)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(outcome.name.as_bytes(), "テスト資料_2026.bin".as_bytes());
    assert_eq!(
        outcome
            .path
            .as_ref()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
        "テスト資料_2026.bin".as_bytes()
    );
    assert!(!temp.path().join("______.bin").exists());
}

#[tokio::test]
async fn download_size_mismatch_keeps_part_file() {
    let body = binary_body(10 * 1024);
    let server_uri = start_raw_mismatch_server(
        include_str!("fixtures/single_basic.html")
            .as_bytes()
            .to_vec(),
        body,
        12 * 1024,
    );
    let temp = TempDir::new().unwrap();
    let opts = DownloadOptions {
        url: format!("{server_uri}/{FILE_ID}"),
        output: Some(temp.path().to_owned()),
        force: false,
        timeout: Duration::from_secs(60),
        retries: 0,
        user_agent: None,
        dump_page: None,
        no_resume: false,
        key: None,
        quiet: true,
        allow_any_host: true,
    };

    let error = download(opts).await.expect_err("size mismatch should fail");

    match error {
        GfileError::SizeMismatch { expected, actual } => {
            assert_eq!(expected, 12 * 1024);
            assert_eq!(actual, 10 * 1024);
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(temp.path().join("example file.bin.part").exists());
    assert!(temp.path().join("example file.bin.part.json").exists());
    assert!(!temp.path().join("example file.bin").exists());
}

#[tokio::test]
async fn download_html_response_is_not_written_to_disk() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><html><body>not a file</body></html>",
            "text/html",
        ))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();

    let error = download(options(&server, &temp, 0))
        .await
        .expect_err("HTML response should fail");

    assert!(matches!(error, GfileError::Parse { .. }));
    assert!(!temp.path().join("example file.bin").exists());
    assert!(!temp.path().join("example file.bin.part").exists());
}

#[tokio::test]
async fn download_retries_503_then_succeeds() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(4096);
    let counter = Arc::new(AtomicUsize::new(0));
    let responder_counter = Arc::clone(&counter);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |_request: &Request| {
            let attempt = responder_counter.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", body.len().to_string())
                    .set_body_bytes(body.clone())
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();

    let report = download(options(&server, &temp, 3)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(outcome.bytes, Some(4096));
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn download_retry_exhaustion_returns_http_status() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();

    let error = download(options(&server, &temp, 1))
        .await
        .expect_err("503 should fail after retries");

    assert!(matches!(error, GfileError::HttpStatus { status: 503, .. }));
    let requests = server.received_requests().await.unwrap();
    let download_requests = requests
        .iter()
        .filter(|request| request.url.path() == "/download.php")
        .count();
    assert_eq!(download_requests, 2);
}

#[tokio::test]
async fn download_without_content_length_succeeds() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(2048);
    mount_file(&server, 200, body, None, None).await;
    let temp = TempDir::new().unwrap();

    let report = download(options(&server, &temp, 0)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(outcome.bytes, Some(2048));
    assert!(outcome.path.as_ref().unwrap().exists());
}

#[tokio::test]
async fn download_resume_206_appends_and_marks_resumed() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, b"hello").unwrap();
    write_sidecar(&sidecar_path, FILE_ID, Some(10), false);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .and(header("Range", "bytes=5-"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 5-9/10")
                .insert_header("Content-Length", "5")
                .insert_header("Content-Type", "application/octet-stream")
                .set_body_bytes(b"world".to_vec()),
        )
        .mount(&server)
        .await;

    let report = download(options(&server, &temp, 0)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(&final_path).unwrap(), b"helloworld");
    assert!(outcome.resumed);
    assert_eq!(outcome.bytes, Some(10));
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[tokio::test]
async fn download_resume_200_truncates_and_redownloads() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, b"hello").unwrap();
    write_sidecar(&sidecar_path, FILE_ID, Some(10), false);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "10")
                .insert_header("Content-Type", "application/octet-stream")
                .set_body_bytes(b"helloworld".to_vec()),
        )
        .mount(&server)
        .await;

    let report = download(options(&server, &temp, 0)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(&final_path).unwrap(), b"helloworld");
    assert!(!outcome.resumed);
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[tokio::test]
async fn download_resume_416_promotes_completed_part() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, b"helloworld").unwrap();
    write_sidecar(&sidecar_path, FILE_ID, Some(10), false);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .and(header("Range", "bytes=10-"))
        .respond_with(ResponseTemplate::new(416))
        .mount(&server)
        .await;

    let report = download(options(&server, &temp, 0)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(&final_path).unwrap(), b"helloworld");
    assert!(outcome.resumed);
    assert_eq!(outcome.bytes, Some(10));
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[tokio::test]
async fn download_bad_sidecar_restarts_from_zero_without_range() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, b"old").unwrap();
    std::fs::write(&sidecar_path, b"not json").unwrap();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "10")
                .insert_header("Content-Type", "application/octet-stream")
                .set_body_bytes(b"helloworld".to_vec()),
        )
        .mount(&server)
        .await;

    let report = download(options(&server, &temp, 0)).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(&final_path).unwrap(), b"helloworld");
    assert!(!outcome.resumed);
    let requests = server.received_requests().await.unwrap();
    let file_request = requests
        .iter()
        .find(|request| request.url.path() == "/download.php")
        .unwrap();
    assert!(file_request.headers.get("range").is_none());
}

#[tokio::test]
async fn download_matomete_continues_after_failure_and_keeps_serial_order() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/matomete_two_files.html")).await;
    let order = Arc::new(AtomicUsize::new(0));
    let first_order = Arc::clone(&order);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |_request: &Request| {
            assert_eq!(first_order.fetch_add(1, Ordering::SeqCst), 0);
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "5")
                .insert_header("Content-Type", "application/octet-stream")
                .insert_header(
                    "Content-Disposition",
                    "attachment; filename*=UTF-8''example%20file.bin",
                )
                .set_body_bytes(b"first".to_vec())
        })
        .mount(&server)
        .await;
    let second_order = Arc::clone(&order);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", "0123abcd-000000example-2"))
        .respond_with(move |_request: &Request| {
            assert_eq!(second_order.fetch_add(1, Ordering::SeqCst), 1);
            ResponseTemplate::new(503)
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.output = Some(temp.path().to_owned());

    let report = download(opts).await.unwrap();

    assert_eq!(report.files.len(), 2);
    assert_eq!(report.failed, 1);
    assert_eq!(report.first_failure_exit_code(), Some(12));
    assert_eq!(
        std::fs::read(temp.path().join("example file.bin")).unwrap(),
        b"first"
    );
    assert!(!temp.path().join("______.bin").exists());
    assert_eq!(order.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn download_stall_timeout_retries_and_succeeds() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(1024);
    let counter = Arc::new(AtomicUsize::new(0));
    let responder_counter = Arc::clone(&counter);
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |_request: &Request| {
            let attempt = responder_counter.fetch_add(1, Ordering::SeqCst);
            let response = ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .set_body_bytes(body.clone());
            if attempt == 0 {
                response.set_delay(Duration::from_secs(2))
            } else {
                response
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 1);
    opts.timeout = Duration::from_secs(1);

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(outcome.bytes, Some(1024));
    assert_eq!(counter.load(Ordering::SeqCst), 2);
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
    status: u16,
    body: Vec<u8>,
    content_length: Option<usize>,
    content_type: Option<&str>,
) {
    let mut response = ResponseTemplate::new(status).set_body_bytes(body);
    if let Some(content_length) = content_length {
        response = response.insert_header("Content-Length", content_length.to_string());
    }
    if let Some(content_type) = content_type {
        response = response.insert_header("Content-Type", content_type);
    }
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(response)
        .mount(server)
        .await;
}

fn options(server: &MockServer, temp: &TempDir, retries: u32) -> DownloadOptions {
    DownloadOptions {
        url: format!("{}/{FILE_ID}", server.uri()),
        output: Some(temp.path().to_owned()),
        force: false,
        no_resume: false,
        key: None,
        timeout: Duration::from_secs(60),
        retries,
        user_agent: None,
        dump_page: None,
        quiet: true,
        allow_any_host: true,
    }
}

fn only_file(report: &DownloadReport) -> &rgfile::download::DownloadFileRecord {
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.failed, 0);
    &report.files[0]
}

fn binary_body(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn write_sidecar(path: &std::path::Path, file_id: &str, expected: Option<u64>, key_used: bool) {
    std::fs::write(
        path,
        serde_json::json!({
            "version": 1,
            "file_id": file_id,
            "expected": expected,
            "key_used": key_used
        })
        .to_string(),
    )
    .unwrap();
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
