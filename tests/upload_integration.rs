// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rgfile::{
    error::GfileError,
    upload::{MIN_CHUNK_SIZE, UploadOptions, upload},
};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";

#[tokio::test]
async fn upload_sends_expected_multipart_fields() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload_success(&server, format!("{}/{FILE_ID}", server.uri())).await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "example file.bin", b"hello upload");

    let report = upload(options(&server, file, true, 0)).await.unwrap();

    assert_eq!(report.bytes, 12);
    assert_eq!(report.verified, None);
    let requests = upload_requests(&server).await;
    assert_eq!(requests.len(), 1);
    let body = &requests[0].body;
    assert_eq!(multipart_text_field(body, "name"), "example file.bin");
    assert_eq!(multipart_text_field(body, "chunk"), "0");
    assert_eq!(multipart_text_field(body, "chunks"), "1");
    assert_eq!(multipart_text_field(body, "lifetime"), "100");
    let id = multipart_text_field(body, "id");
    assert_eq!(id.len(), 32);
    assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert!(String::from_utf8_lossy(body).contains("name=\"file\""));
    assert!(String::from_utf8_lossy(body).contains("filename=\"blob\""));
    assert!(body_contains(body, b"hello upload"));
}

#[tokio::test]
async fn upload_sends_chunks_serially_from_zero() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    let order = Arc::new(AtomicUsize::new(0));
    let responder_order = Arc::clone(&order);
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |request: &Request| {
            let expected = responder_order.fetch_add(1, Ordering::SeqCst);
            let chunk = multipart_text_field(&request.body, "chunk")
                .parse::<usize>()
                .unwrap();
            assert_eq!(chunk, expected);
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "status": 0, "url": format!("http://example.invalid/{FILE_ID}") }))
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut body = vec![b'a'; MIN_CHUNK_SIZE as usize];
    body.extend(vec![b'b'; MIN_CHUNK_SIZE as usize]);
    body.extend(b"tail");
    let file = write_file(&temp, "chunks.bin", &body);

    let report = upload(options(&server, file, true, 0)).await.unwrap();

    assert_eq!(report.bytes, body.len() as u64);
    assert_eq!(order.load(Ordering::SeqCst), 3);
    let requests = upload_requests(&server).await;
    assert_eq!(requests.len(), 3);
    assert!(body_contains(&requests[0].body, &[b'a'; 128]));
    assert!(body_contains(&requests[1].body, &[b'b'; 128]));
    assert!(body_contains(&requests[2].body, b"tail"));
}

#[tokio::test]
async fn upload_reuses_cookie_from_first_chunk() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    let saw_cookie = Arc::new(AtomicBool::new(false));
    let responder_saw_cookie = Arc::clone(&saw_cookie);
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |request: &Request| {
            let chunk = multipart_text_field(&request.body, "chunk");
            if chunk == "0" {
                ResponseTemplate::new(200)
                    .insert_header("Set-Cookie", "session=EXAMPLE_SESSION; Path=/")
                    .set_body_json(serde_json::json!({ "status": 0 }))
            } else {
                let cookie = request
                    .headers
                    .get("cookie")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                if cookie.contains("session=EXAMPLE_SESSION") {
                    responder_saw_cookie.store(true, Ordering::SeqCst);
                }
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "status": 0, "url": format!("http://example.invalid/{FILE_ID}") }))
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(
        &temp,
        "cookie.bin",
        &vec![b'x'; MIN_CHUNK_SIZE as usize + 1],
    );

    upload(options(&server, file, true, 0)).await.unwrap();

    assert!(saw_cookie.load(Ordering::SeqCst));
}

#[tokio::test]
async fn upload_missing_final_url_is_upload_rejected() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": 0 })))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "missing-url.bin", b"hello");

    let error = upload(options(&server, file, true, 0))
        .await
        .expect_err("missing final url should fail");

    assert!(matches!(error, GfileError::UploadRejected { .. }));
}

#[tokio::test]
async fn upload_retries_5xx_then_succeeds() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    let counter = Arc::new(AtomicUsize::new(0));
    let responder_counter = Arc::clone(&counter);
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |_request: &Request| {
            let attempt = responder_counter.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "status": 0, "url": format!("http://example.invalid/{FILE_ID}") }))
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "retry.bin", b"hello");

    upload(options(&server, file, true, 2)).await.unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn upload_idle_timeout_retries_stalled_response_then_succeeds() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    let counter = Arc::new(AtomicUsize::new(0));
    let responder_counter = Arc::clone(&counter);
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |_request: &Request| {
            let attempt = responder_counter.fetch_add(1, Ordering::SeqCst);
            let response = ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "status": 0, "url": format!("http://example.invalid/{FILE_ID}") }));
            if attempt == 0 {
                response.set_delay(Duration::from_secs(2))
            } else {
                response
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "idle-timeout.bin", b"hello");

    upload(options(&server, file, true, 1)).await.unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn upload_does_not_retry_4xx_or_continue_later_chunks() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(ResponseTemplate::new(413))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "stop.bin", &vec![b'x'; MIN_CHUNK_SIZE as usize + 1]);

    let error = upload(options(&server, file, true, 3))
        .await
        .expect_err("4xx should fail without retry");

    assert!(matches!(error, GfileError::UploadRejected { .. }));
    assert_eq!(upload_requests(&server).await.len(), 1);
}

#[tokio::test]
async fn upload_verify_success_uses_head_content_length() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload_success(&server, format!("{}/{FILE_ID}", server.uri())).await;
    mount_download_page(&server).await;
    Mock::given(method("HEAD"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5"))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "verify.bin", b"hello");

    let report = upload(options(&server, file, false, 0)).await.unwrap();

    assert_eq!(report.verified, Some(true));
}

#[tokio::test]
async fn upload_verify_failure_returns_verify_failed() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload_success(&server, format!("{}/{FILE_ID}", server.uri())).await;
    mount_download_page(&server).await;
    Mock::given(method("HEAD"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "9"))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "verify-fail.bin", b"hello");

    let error = upload(options(&server, file, false, 0))
        .await
        .expect_err("mismatched verification size should fail");

    match error {
        GfileError::VerifyFailed { expected, actual } => {
            assert_eq!(expected, 5);
            assert_eq!(actual, 9);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn upload_verify_falls_back_to_get_headers_without_reading_body() {
    let raw_url = start_head_fallback_server();
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload_success(&server, format!("{raw_url}/{FILE_ID}")).await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "fallback.bin", b"hello");
    let start = Instant::now();

    let report = upload(options(&server, file, false, 0)).await.unwrap();

    assert_eq!(report.verified, Some(true));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "GET fallback consumed or waited for the response body"
    );
}

#[tokio::test]
async fn upload_verify_unavailable_reports_null_verified() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload_success(&server, format!("{}/{FILE_ID}", server.uri())).await;
    mount_download_page(&server).await;
    Mock::given(method("HEAD"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ignored".to_vec()))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "unavailable.bin", b"hello");

    let report = upload(options(&server, file, false, 0)).await.unwrap();

    assert_eq!(report.verified, None);
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

async fn mount_upload_success(server: &MockServer, url: String) {
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |_request: &Request| {
            ResponseTemplate::new(200).set_body_json(upload_success_json(&url))
        })
        .mount(server)
        .await;
}

async fn mount_download_page(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/single_basic.html"), "text/html"),
        )
        .mount(server)
        .await;
}

fn options(
    server: &MockServer,
    file: std::path::PathBuf,
    no_verify: bool,
    retries: u32,
) -> UploadOptions {
    UploadOptions {
        file,
        chunk_size: MIN_CHUNK_SIZE,
        verify: !no_verify,
        timeout: Duration::from_secs(1),
        retries,
        quiet: true,
        allow_any_host: true,
        entry_url: server.uri(),
        ..UploadOptions::default()
    }
}

fn write_file(temp: &TempDir, name: &str, body: &[u8]) -> std::path::PathBuf {
    let path = temp.path().join(name);
    std::fs::write(&path, body).unwrap();
    path
}

async fn upload_requests(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/upload_chunk.php")
        .collect()
}

fn multipart_text_field(body: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(body);
    let marker = format!("name=\"{name}\"\r\n\r\n");
    let start = text.find(&marker).expect("multipart field marker") + marker.len();
    let end = text[start..].find("\r\n--").expect("multipart field end") + start;
    text[start..end].to_owned()
}

fn body_contains(body: &[u8], needle: &[u8]) -> bool {
    body.windows(needle.len()).any(|window| window == needle)
}

fn upload_success_json(url: &str) -> serde_json::Value {
    let text = include_str!("fixtures/upload_chunk_success.json")
        .replace("https://99.gigafile.nu/0123abcd-000000example", url);
    serde_json::from_str(&text).unwrap()
}

fn start_head_fallback_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(3) {
            let mut stream = stream.unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            if request.starts_with(&format!("GET /{FILE_ID} ")) {
                let body = include_str!("fixtures/single_basic.html").as_bytes();
                write_response(&mut stream, 200, "text/html", Some(body.len()), body);
            } else if request.starts_with("HEAD /download.php") {
                write_response(&mut stream, 405, "text/plain", Some(0), b"");
            } else if request.starts_with("GET /download.php") {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.flush().unwrap();
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    });
    format!("http://{addr}")
}

fn write_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_type: &str,
    len: Option<usize>,
    body: &[u8],
) {
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\n"
    )
    .unwrap();
    if let Some(len) = len {
        write!(stream, "Content-Length: {len}\r\n").unwrap();
    }
    write!(stream, "Connection: close\r\n\r\n").unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}
