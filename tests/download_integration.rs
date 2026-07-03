// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io::{Read, Seek, SeekFrom, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rgfile::{
    download::{DownloadOptions, DownloadReport, FileSelection, download},
    error::GfileError,
};
use sha2::{Digest, Sha256};
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
        selection: None,
        threads: 1,
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
async fn download_range_header_is_sent_title_cased_for_legacy_servers() {
    // Live 2026-07-03: GigaFile matches header names case-sensitively and
    // ignores hyper's default lowercase `range:`, replying 200 instead of 206.
    let (server_uri, captured) = start_header_capture_server(
        include_str!("fixtures/single_basic.html")
            .as_bytes()
            .to_vec(),
        binary_body(2 * 1024),
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
        selection: None,
        threads: 2,
        quiet: true,
        allow_any_host: true,
    };

    let _ = download(opts).await;

    let request = captured.lock().unwrap().clone();
    assert!(
        request.contains("\r\nRange: bytes=0-"),
        "download request must send a title-cased Range header, got:\n{request}"
    );
    assert!(
        !request.contains("\r\nrange:"),
        "lowercase range header is ignored by the live server, got:\n{request}"
    );
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
async fn download_threads_four_ranges_and_matches_sha256() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(64 * 1024 + 7);
    let expected_hash = sha256_hex(&body);
    let observed_ranges = Arc::new(Mutex::new(Vec::new()));
    let non_range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&observed_ranges);
    let responder_non_ranges = Arc::clone(&non_range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                responder_ranges.lock().unwrap().push((start, end));
                range_response(&responder_body, start, end)
            } else {
                responder_non_ranges.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(outcome.bytes, Some(body.len() as u64));
    assert_eq!(outcome.threads, Some(4));
    let downloaded = std::fs::read(outcome.path.as_ref().unwrap()).unwrap();
    assert_eq!(sha256_hex(&downloaded), expected_hash);
    let mut ranges = observed_ranges.lock().unwrap().clone();
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        expected_ranges_after_initial(body.len() as u64, 4, 2559)
    );
    assert_eq!(non_range_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_threads_uses_content_disposition_from_first_206() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_masked.html")).await;
    let body = binary_body(4096);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                let mut response = range_response(&responder_body, start, end);
                if start == 0 {
                    response = response.insert_header(
                        "Content-Disposition",
                        "attachment; filename*=UTF-8''%E3%83%86%E3%82%B9%E3%83%88%E8%B3%87%E6%96%99_2026.bin",
                    );
                }
                response
            } else {
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
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
    assert_eq!(std::fs::read(outcome.path.as_ref().unwrap()).unwrap(), body);
}

#[tokio::test]
async fn download_segment_retries_5xx_then_succeeds() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(32 * 1024);
    let first_range_failures = Arc::new(AtomicUsize::new(0));
    let responder_failures = Arc::clone(&first_range_failures);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                if responder_failures.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                } else {
                    range_response(&responder_body, start, end)
                }
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
    let mut opts = options(&server, &temp, 1);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(outcome.path.as_ref().unwrap()).unwrap(), body);
    assert_eq!(first_range_failures.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn download_threads_falls_back_when_range_returns_200() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(16 * 1024);
    let range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if range_header(request).is_some() {
                responder_ranges.fetch_add(1, Ordering::SeqCst);
            }
            ResponseTemplate::new(200)
                .insert_header("Content-Length", responder_body.len().to_string())
                .insert_header("Content-Type", "application/octet-stream")
                .set_body_bytes(responder_body.clone())
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(outcome.path.as_ref().unwrap()).unwrap(), body);
    assert_eq!(outcome.threads, Some(1));
    assert_eq!(range_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn download_threads_resumes_v2_sidecar_segments() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(64 * 1024);
    let ranges = expected_ranges(body.len() as u64, 4);
    let partial = 1024_u64;
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, vec![0_u8; body.len()]).unwrap();
    write_body_range(&part_path, &body, ranges[0].0, ranges[0].1).unwrap();
    write_body_range(&part_path, &body, ranges[1].0, ranges[1].0 + partial - 1).unwrap();
    write_segment_sidecar(
        &sidecar_path,
        FILE_ID,
        body.len() as u64,
        false,
        &[
            (
                ranges[0].0,
                ranges[0].1,
                true,
                ranges[0].1 - ranges[0].0 + 1,
            ),
            (ranges[1].0, ranges[1].1, false, partial),
            (ranges[2].0, ranges[2].1, false, 0),
            (ranges[3].0, ranges[3].1, false, 0),
        ],
    );
    let observed_ranges = Arc::new(Mutex::new(Vec::new()));
    let non_range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&observed_ranges);
    let responder_non_ranges = Arc::clone(&non_range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                responder_ranges.lock().unwrap().push((start, end));
                range_response(&responder_body, start, end)
            } else {
                responder_non_ranges.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert!(outcome.resumed);
    assert_eq!(std::fs::read(&final_path).unwrap(), body);
    let mut observed = observed_ranges.lock().unwrap().clone();
    observed.sort_unstable();
    assert_eq!(
        observed,
        vec![(ranges[1].0 + partial, ranges[1].1), ranges[2], ranges[3],]
    );
    assert_eq!(non_range_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_threads_resume_all_partial_segments_offset_by_downloaded() {
    // Every segment is partially downloaded (none `done`), matching the live
    // regression where the bootstrap probe (segment 0) AND the worker segments
    // all carry a nonzero `downloaded`. Each incomplete segment must resume at
    // `start + downloaded`, never re-fetching bytes it already has.
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(64 * 1024);
    let ranges = expected_ranges(body.len() as u64, 4);
    let downloaded = [4096_u64, 8192, 2048, 1024];
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, vec![0_u8; body.len()]).unwrap();
    for (range, done) in ranges.iter().zip(downloaded) {
        write_body_range(&part_path, &body, range.0, range.0 + done - 1).unwrap();
    }
    write_segment_sidecar(
        &sidecar_path,
        FILE_ID,
        body.len() as u64,
        false,
        &[
            (ranges[0].0, ranges[0].1, false, downloaded[0]),
            (ranges[1].0, ranges[1].1, false, downloaded[1]),
            (ranges[2].0, ranges[2].1, false, downloaded[2]),
            (ranges[3].0, ranges[3].1, false, downloaded[3]),
        ],
    );
    let observed_ranges = Arc::new(Mutex::new(Vec::new()));
    let non_range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&observed_ranges);
    let responder_non_ranges = Arc::clone(&non_range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                responder_ranges.lock().unwrap().push((start, end));
                range_response(&responder_body, start, end)
            } else {
                responder_non_ranges.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert!(outcome.resumed);
    assert_eq!(std::fs::read(&final_path).unwrap(), body);
    let expected_offsets: Vec<(u64, u64)> = ranges
        .iter()
        .zip(downloaded)
        .map(|(range, done)| (range.0 + done, range.1))
        .collect();
    let mut observed = observed_ranges.lock().unwrap().clone();
    observed.sort_unstable();
    assert_eq!(observed, expected_offsets);
    // No request may re-fetch bytes below `start + downloaded` of any segment.
    for (range, done) in ranges.iter().zip(downloaded) {
        let resume_at = range.0 + done;
        for (start, end) in observed.iter().copied() {
            if start <= range.1 && end >= range.0 {
                assert!(
                    start >= resume_at,
                    "segment {range:?} re-fetched from {start}, below resume point {resume_at}"
                );
            }
        }
    }
    assert_eq!(non_range_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_threads_resume_completes_hash_without_underfetch() {
    // Mixes a completed segment with partially-downloaded workers and a fresh
    // worker, over a non-power-of-two length. A completed segment must not be
    // re-requested; every other segment must resume at `start + downloaded`;
    // the finished file must match the source hash bit-for-bit.
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(40_000);
    let expected_hash = sha256_hex(&body);
    let ranges = expected_ranges(body.len() as u64, 4);
    // seg0 done, seg1/seg2 partial, seg3 untouched.
    let downloaded = [
        ranges[0].1 - ranges[0].0 + 1, // full
        3000_u64,
        100_u64,
        0_u64,
    ];
    let done = [true, false, false, false];
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, vec![0_u8; body.len()]).unwrap();
    for (range, got) in ranges.iter().zip(downloaded) {
        if got > 0 {
            write_body_range(&part_path, &body, range.0, range.0 + got - 1).unwrap();
        }
    }
    write_segment_sidecar(
        &sidecar_path,
        FILE_ID,
        body.len() as u64,
        false,
        &[
            (ranges[0].0, ranges[0].1, done[0], downloaded[0]),
            (ranges[1].0, ranges[1].1, done[1], downloaded[1]),
            (ranges[2].0, ranges[2].1, done[2], downloaded[2]),
            (ranges[3].0, ranges[3].1, done[3], downloaded[3]),
        ],
    );
    let observed_ranges = Arc::new(Mutex::new(Vec::new()));
    let non_range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&observed_ranges);
    let responder_non_ranges = Arc::clone(&non_range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                responder_ranges.lock().unwrap().push((start, end));
                range_response(&responder_body, start, end)
            } else {
                responder_non_ranges.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert!(outcome.resumed);
    let finished = std::fs::read(&final_path).unwrap();
    assert_eq!(sha256_hex(&finished), expected_hash);
    let observed = observed_ranges.lock().unwrap().clone();
    // The completed segment must never be re-requested.
    assert!(
        !observed
            .iter()
            .any(|(start, _)| (ranges[0].0..=ranges[0].1).contains(start)),
        "completed segment was re-requested: {observed:?}"
    );
    // No request may re-fetch bytes below `start + downloaded` of any segment.
    for (range, got) in ranges.iter().zip(downloaded) {
        let resume_at = range.0 + got;
        for (start, end) in observed.iter().copied() {
            if start <= range.1 && end >= range.0 {
                assert!(
                    start >= resume_at,
                    "segment {range:?} re-fetched from {start}, below resume point {resume_at}"
                );
            }
        }
    }
    assert_eq!(non_range_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_threads_resume_200_consumes_once_and_clears_segments() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(16 * 1024);
    let ranges = expected_ranges(body.len() as u64, 4);
    let partial = 512_u64;
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, vec![0_u8; body.len()]).unwrap();
    write_body_range(&part_path, &body, ranges[0].0, ranges[0].1).unwrap();
    write_body_range(&part_path, &body, ranges[1].0, ranges[1].0 + partial - 1).unwrap();
    write_segment_sidecar(
        &sidecar_path,
        FILE_ID,
        body.len() as u64,
        false,
        &[
            (
                ranges[0].0,
                ranges[0].1,
                true,
                ranges[0].1 - ranges[0].0 + 1,
            ),
            (ranges[1].0, ranges[1].1, false, partial),
            (ranges[2].0, ranges[2].1, false, 0),
            (ranges[3].0, ranges[3].1, false, 0),
        ],
    );
    let observed_ranges = Arc::new(Mutex::new(Vec::new()));
    let non_range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&observed_ranges);
    let responder_non_ranges = Arc::clone(&non_range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some(range) = range_header(request) {
                responder_ranges.lock().unwrap().push(range);
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", responder_body.len().to_string())
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(responder_body.clone())
            } else {
                responder_non_ranges.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(&final_path).unwrap(), body);
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
    assert!(!outcome.resumed);
    assert_eq!(outcome.threads, Some(1));
    assert_eq!(
        observed_ranges.lock().unwrap().as_slice(),
        &[(ranges[1].0 + partial, ranges[1].1)]
    );
    assert_eq!(non_range_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_threads_discards_v1_sidecar_and_restarts_segmented() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(24 * 1024);
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, b"old").unwrap();
    write_sidecar(&sidecar_path, FILE_ID, Some(body.len() as u64), false);
    let observed_ranges = Arc::new(Mutex::new(Vec::new()));
    let non_range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&observed_ranges);
    let responder_non_ranges = Arc::clone(&non_range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                responder_ranges.lock().unwrap().push((start, end));
                range_response(&responder_body, start, end)
            } else {
                responder_non_ranges.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert!(!outcome.resumed);
    assert_eq!(std::fs::read(&final_path).unwrap(), body);
    let mut ranges = observed_ranges.lock().unwrap().clone();
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        expected_ranges_after_initial(body.len() as u64, 4, 2559)
    );
    assert_eq!(non_range_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_no_resume_clears_v2_segment_progress() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(32 * 1024);
    let ranges = expected_ranges(body.len() as u64, 4);
    let temp = TempDir::new().unwrap();
    let final_path = temp.path().join("example file.bin");
    let part_path = temp.path().join("example file.bin.part");
    let sidecar_path = temp.path().join("example file.bin.part.json");
    std::fs::write(&part_path, vec![0_u8; body.len()]).unwrap();
    write_body_range(&part_path, &body, ranges[0].0, ranges[0].1).unwrap();
    write_segment_sidecar(
        &sidecar_path,
        FILE_ID,
        body.len() as u64,
        false,
        &[
            (
                ranges[0].0,
                ranges[0].1,
                true,
                ranges[0].1 - ranges[0].0 + 1,
            ),
            (ranges[1].0, ranges[1].1, false, 0),
            (ranges[2].0, ranges[2].1, false, 0),
            (ranges[3].0, ranges[3].1, false, 0),
        ],
    );
    let observed_ranges = Arc::new(Mutex::new(Vec::new()));
    let non_range_requests = Arc::new(AtomicUsize::new(0));
    let responder_ranges = Arc::clone(&observed_ranges);
    let responder_non_ranges = Arc::clone(&non_range_requests);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some((start, end)) = range_header(request) {
                responder_ranges.lock().unwrap().push((start, end));
                range_response(&responder_body, start, end)
            } else {
                responder_non_ranges.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;
    opts.no_resume = true;

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert!(!outcome.resumed);
    assert_eq!(std::fs::read(&final_path).unwrap(), body);
    let mut observed = observed_ranges.lock().unwrap().clone();
    observed.sort_unstable();
    assert_eq!(
        observed,
        expected_ranges_after_initial(body.len() as u64, 4, 2559)
    );
    assert_eq!(non_range_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_threads_send_dlkey_on_every_segment() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    let body = binary_body(16 * 1024);
    let keyed_download_requests = Arc::new(AtomicUsize::new(0));
    let responder_keyed = Arc::clone(&keyed_download_requests);
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
    let mut opts = options(&server, &temp, 0);
    opts.threads = 4;
    opts.key = Some("EXAMPLE-KEY-0000".to_owned());

    let report = download(opts).await.unwrap();
    let outcome = only_file(&report);

    assert_eq!(std::fs::read(outcome.path.as_ref().unwrap()).unwrap(), body);
    assert_eq!(keyed_download_requests.load(Ordering::SeqCst), 4);
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
async fn download_matomete_selects_subset_only() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/matomete_two_files.html")).await;
    mount_named_file(
        &server,
        "0123abcd-000000example-2",
        "selected.bin",
        b"second".to_vec(),
    )
    .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.selection = Some(FileSelection::parse("2").unwrap());

    let report = download(opts).await.unwrap();

    assert_eq!(report.files.len(), 1);
    assert_eq!(report.failed, 0);
    assert_eq!(
        std::fs::read(temp.path().join("selected.bin")).unwrap(),
        b"second"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|request| {
        request.url.path() == "/download.php"
            && request
                .url
                .query_pairs()
                .any(|(key, value)| key == "file" && value == FILE_ID)
    }));
}

#[tokio::test]
async fn download_matomete_select_dedupes_and_keeps_page_order() {
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
                    "attachment; filename*=UTF-8''first.bin",
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
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "6")
                .insert_header("Content-Type", "application/octet-stream")
                .insert_header(
                    "Content-Disposition",
                    "attachment; filename*=UTF-8''second.bin",
                )
                .set_body_bytes(b"second".to_vec())
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.selection = Some(FileSelection::parse("2,1,2").unwrap());

    let report = download(opts).await.unwrap();

    assert_eq!(report.files.len(), 2);
    assert_eq!(report.failed, 0);
    assert_eq!(
        std::fs::read(temp.path().join("first.bin")).unwrap(),
        b"first"
    );
    assert_eq!(
        std::fs::read(temp.path().join("second.bin")).unwrap(),
        b"second"
    );
    assert_eq!(order.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn download_single_accepts_only_select_one() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/single_basic.html")).await;
    mount_file(&server, 200, b"one".to_vec(), Some(3), None).await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.selection = Some(FileSelection::parse("1").unwrap());

    let report = download(opts).await.unwrap();
    assert_eq!(only_file(&report).bytes, Some(3));

    let mut opts = options(&server, &temp, 0);
    opts.selection = Some(FileSelection::parse("2").unwrap());
    let error = download(opts).await.unwrap_err();
    assert_eq!(error.exit_code(), 2);
    assert!(
        error
            .user_message()
            .contains("single-file pages only accept")
    );
    assert!(error.user_message().contains("rgfile info"));
}

#[tokio::test]
async fn download_matomete_select_combines_with_key_and_threads() {
    let server = MockServer::start().await;
    mount_page(&server, include_str!("fixtures/matomete_two_files.html")).await;
    let body = binary_body(12 * 1024);
    let request_count = Arc::new(AtomicUsize::new(0));
    let responder_count = Arc::clone(&request_count);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .and(query_param("dlkey", "EXAMPLE-KEY-0000"))
        .respond_with(move |request: &Request| {
            responder_count.fetch_add(1, Ordering::SeqCst);
            if let Some((start, end)) = range_header(request) {
                let mut response = range_response(&responder_body, start, end);
                if start == 0 {
                    response = response.insert_header(
                        "Content-Disposition",
                        "attachment; filename*=UTF-8''threaded.bin",
                    );
                }
                response
            } else {
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let mut opts = options(&server, &temp, 0);
    opts.key = Some("EXAMPLE-KEY-0000".to_owned());
    opts.selection = Some(FileSelection::parse("1").unwrap());
    opts.threads = 4;

    let report = download(opts).await.unwrap();

    let outcome = only_file(&report);
    assert_eq!(outcome.threads, Some(4));
    assert_eq!(
        std::fs::read(temp.path().join("threaded.bin")).unwrap(),
        body
    );
    assert_eq!(request_count.load(Ordering::SeqCst), 4);
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|request| {
        request.url.path() == "/download.php"
            && request
                .url
                .query_pairs()
                .any(|(key, value)| key == "file" && value == "0123abcd-000000example-2")
    }));
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

async fn mount_named_file(server: &MockServer, file_id: &str, name: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", file_id))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .insert_header("Content-Type", "application/octet-stream")
                .insert_header(
                    "Content-Disposition",
                    format!("attachment; filename*=UTF-8''{name}"),
                )
                .set_body_bytes(body),
        )
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
        selection: None,
        threads: 1,
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

fn write_segment_sidecar(
    path: &std::path::Path,
    file_id: &str,
    expected: u64,
    key_used: bool,
    segments: &[(u64, u64, bool, u64)],
) {
    let segments: Vec<_> = segments
        .iter()
        .map(|(start, end, done, downloaded)| {
            serde_json::json!({
                "start": start,
                "end": end,
                "done": done,
                "downloaded": downloaded,
            })
        })
        .collect();
    std::fs::write(
        path,
        serde_json::json!({
            "version": 2,
            "file_id": file_id,
            "expected": expected,
            "key_used": key_used,
            "segments": segments,
        })
        .to_string(),
    )
    .unwrap();
}

fn write_body_range(
    path: &std::path::Path,
    body: &[u8],
    start: u64,
    end: u64,
) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(start))?;
    file.write_all(&body[start as usize..=end as usize])
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

fn expected_ranges(len: u64, threads: u8) -> Vec<(u64, u64)> {
    let count = u64::from(threads).min(len).max(1);
    let base = len / count;
    let remainder = len % count;
    let mut start = 0;
    let mut ranges = Vec::new();
    for index in 0..count {
        let segment_len = base + if index < remainder { 1 } else { 0 };
        let end = start + segment_len - 1;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

fn expected_ranges_after_initial(len: u64, threads: u8, initial_end: u64) -> Vec<(u64, u64)> {
    let first_end = initial_end.min(len - 1);
    let mut ranges = vec![(0, first_end)];
    let mut start = first_end + 1;
    if start >= len {
        return ranges;
    }
    let remaining_threads = u64::from(threads).saturating_sub(1).min(len - start).max(1);
    let remaining = len - start;
    let base = remaining / remaining_threads;
    let remainder = remaining % remaining_threads;
    for index in 0..remaining_threads {
        let segment_len = base + if index < remainder { 1 } else { 0 };
        let end = start + segment_len - 1;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

fn sha256_hex(body: &[u8]) -> String {
    format!("{:x}", Sha256::digest(body))
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

fn start_header_capture_server(
    page_body: Vec<u8>,
    file_body: Vec<u8>,
) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_writer = Arc::clone(&captured);
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            if request.starts_with(&format!("GET /{FILE_ID} ")) {
                write_response(&mut stream, "text/html", page_body.len(), &page_body);
            } else {
                *captured_writer.lock().unwrap() = request.clone();
                write_response(
                    &mut stream,
                    "application/octet-stream",
                    file_body.len(),
                    &file_body,
                );
            }
        }
    });
    (format!("http://{addr}"), captured)
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
