// SPDX-License-Identifier: GPL-3.0-only

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";

#[tokio::test]
async fn config_download_dir_and_user_agent_apply_and_cli_output_overrides() {
    let server = MockServer::start().await;
    mount_page_with_user_agent(&server, "rgfile-config-test").await;
    mount_file_with_user_agent(&server, "rgfile-config-test", b"hello config".to_vec()).await;
    let temp = TempDir::new().unwrap();
    let config_output = temp.path().join("from-config");
    let cli_output = temp.path().join("from-cli");
    std::fs::create_dir_all(&cli_output).unwrap();
    let config = write_config(
        &temp,
        &format!(
            "[download]\ndir = \"{}\"\n\n[network]\nuser_agent = \"rgfile-config-test\"\ntimeout = 5\nretries = 0\n",
            toml_path(&config_output)
        ),
    );

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["--config"])
        .arg(&config)
        .args(["download", &format!("{}/{FILE_ID}", server.uri())])
        .assert()
        .success();

    assert_eq!(
        std::fs::read(config_output.join("example file.bin")).unwrap(),
        b"hello config"
    );

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["--config"])
        .arg(&config)
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(&cli_output)
        .assert()
        .success();

    assert_eq!(
        std::fs::read(cli_output.join("example file.bin")).unwrap(),
        b"hello config"
    );
}

#[tokio::test]
async fn config_download_threads_applies_and_cli_threads_overrides() {
    let server = MockServer::start().await;
    mount_page(&server).await;
    let body = binary_body(12 * 1024);
    let runs = Arc::new(Mutex::new(Vec::<Vec<(u64, u64)>>::new()));
    let responder_runs = Arc::clone(&runs);
    let responder_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(move |request: &Request| {
            if let Some(range) = range_header(request) {
                let mut runs = responder_runs.lock().unwrap();
                if range.0 == 0 || runs.is_empty() {
                    runs.push(Vec::new());
                }
                runs.last_mut().unwrap().push(range);
                range_response(&responder_body, range.0, range.1)
            } else {
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    let config_output = temp.path().join("from-config");
    let cli_output = temp.path().join("from-cli");
    std::fs::create_dir_all(&cli_output).unwrap();
    let config = write_config(
        &temp,
        &format!(
            "[download]\ndir = \"{}\"\nthreads = 3\n\n[network]\nretries = 0\n",
            toml_path(&config_output)
        ),
    );

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["--config"])
        .arg(&config)
        .args(["download", &format!("{}/{FILE_ID}", server.uri())])
        .assert()
        .success();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args(["--config"])
        .arg(&config)
        .args([
            "download",
            "--threads",
            "2",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(&cli_output)
        .assert()
        .success();

    assert_eq!(
        std::fs::read(config_output.join("example file.bin")).unwrap(),
        body
    );
    assert_eq!(
        std::fs::read(cli_output.join("example file.bin")).unwrap(),
        body
    );
    let runs = runs.lock().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].len(), 3);
    assert_eq!(runs[1].len(), 2);
}

#[tokio::test]
async fn config_upload_lifetime_applies_and_cli_lifetime_overrides() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    let lifetimes =
        mount_upload_collecting_lifetimes(&server, format!("{}/{FILE_ID}", server.uri())).await;
    let temp = TempDir::new().unwrap();
    let file = write_file(&temp, "upload.bin", b"hello upload");
    let config = write_config(&temp, "[upload]\nlifetime = 3\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .args(["--config"])
        .arg(&config)
        .args(["upload", "--no-verify"])
        .arg(&file)
        .assert()
        .success();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .args(["--config"])
        .arg(&config)
        .args(["upload", "--no-verify", "--lifetime", "5"])
        .arg(&file)
        .assert()
        .success();

    assert_eq!(
        lifetimes.lock().unwrap().as_slice(),
        ["3".to_owned(), "5".to_owned()]
    );
}

#[test]
fn config_syntax_error_exits_2_with_line_number() {
    let temp = TempDir::new().unwrap();
    let config = write_config(&temp, "[network]\ntimeout =\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(config)
        .args(["info", "https://23.gigafile.nu/0123abcd-000000example"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("line 2"));
}

#[test]
fn config_path_prints_override_even_when_missing() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("missing").join("config.toml");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(config.display().to_string()))
        .stderr(predicate::str::contains("file does not exist yet"));
}

#[test]
fn config_path_ignores_no_config_flag() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--no-config", "config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config.toml"));
}

#[test]
fn config_show_missing_file_reports_default_and_unset_sources() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("missing.toml");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no config file found"))
        .stdout(predicate::str::contains("download.dir = <unset> (unset)"))
        .stdout(predicate::str::contains("download.threads = 1 (default)"))
        .stdout(predicate::str::contains(
            "network.user_agent = <unset> (unset)",
        ));
}

#[test]
fn config_show_no_config_reports_no_config_source() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--no-config", "config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-config (using defaults)"))
        .stdout(predicate::str::contains("exists: false"))
        .stdout(predicate::str::contains("download.threads = 1 (default)"));
}

#[test]
fn config_show_invalid_config_exits_2_with_line_number() {
    let temp = TempDir::new().unwrap();
    let config = write_config(&temp, "[network]\ntimeout =\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(config)
        .args(["config", "show"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("line 2"));
}

#[test]
fn config_show_file_reports_file_sources() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("downloads");
    let config = write_config(
        &temp,
        &format!(
            "[download]\ndir = \"{}\"\nthreads = 4\n\n[upload]\nlifetime = 7\nthreads = 3\n\n[network]\ntimeout = 9\nretries = 1\nuser_agent = \"rgfile-config-test\"\n\n[history]\nenabled = true\nstore_delete_keys = true\n",
            toml_path(&dir)
        ),
    );

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("loaded from"))
        .stdout(predicate::str::contains("download.threads = 4 (file)"))
        .stdout(predicate::str::contains("upload.lifetime = 7 (file)"))
        .stdout(predicate::str::contains(
            "network.user_agent = rgfile-config-test (file)",
        ))
        .stdout(predicate::str::contains(
            "history.store_delete_keys = true (file)",
        ));
}

#[test]
fn config_show_json_reports_values_and_sources() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("downloads");
    let config = write_config(
        &temp,
        &format!(
            "[download]\ndir = \"{}\"\nthreads = 5\n\n[network]\nretries = 0\n",
            toml_path(&dir)
        ),
    );

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "show", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["exists"], true);
    assert_eq!(value["source"], "file");
    assert_eq!(value["values"]["download"]["dir"]["source"], "file");
    assert_eq!(
        value["values"]["download"]["dir"]["value"],
        dir.display().to_string()
    );
    assert_eq!(value["values"]["download"]["threads"]["value"], 5);
    assert_eq!(value["values"]["network"]["retries"]["source"], "file");
    assert_eq!(value["values"]["network"]["timeout"]["source"], "default");
    assert_eq!(value["values"]["network"]["user_agent"]["source"], "unset");
}

#[test]
fn config_init_defaults_writes_template_that_loads_as_defaults() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("nested").join("config.toml");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "init", "--defaults"])
        .assert()
        .success()
        .stdout(predicate::str::contains(config.display().to_string()))
        .stderr(predicate::str::contains("rgfile config show"));

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("# threads = 1"));
    assert!(text.contains("[network]"));

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "show", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["values"]["download"]["threads"]["source"], "default");
    assert_eq!(value["values"]["upload"]["lifetime"]["value"], 100);
    assert_eq!(value["values"]["history"]["enabled"]["source"], "default");
}

#[test]
fn config_init_defaults_existing_requires_force() {
    let temp = TempDir::new().unwrap();
    let config = write_config(&temp, "[download]\nthreads = 2\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "init", "--defaults"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--force"));

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("threads = 2"));
}

#[test]
fn config_init_defaults_force_overwrites_existing_file() {
    let temp = TempDir::new().unwrap();
    let config = write_config(&temp, "[download]\nthreads = 2\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "init", "--defaults", "--force"])
        .assert()
        .success();

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("# threads = 1"));
    assert!(!text.contains("threads = 2"));
}

#[test]
fn config_init_interactive_non_tty_exits_2() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config.toml");

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--config"])
        .arg(&config)
        .args(["config", "init"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--defaults"));
}

#[tokio::test]
async fn history_default_off_and_no_history_override_do_not_write() {
    let server = MockServer::start().await;
    mount_page(&server).await;
    mount_file(&server, b"hello".to_vec()).await;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let output = temp.path().join("out");
    std::fs::create_dir_all(&output).unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args([
            "--no-config",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(&output)
        .assert()
        .success();
    assert!(!history_path(&data).exists());

    let config = write_config(&temp, "[history]\nenabled = true\n");
    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--config"])
        .arg(config)
        .args([
            "--no-history",
            "download",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(&output)
        .assert()
        .failure();
    assert!(!history_path(&data).exists());
}

#[tokio::test]
async fn history_records_download_without_download_key() {
    let server = MockServer::start().await;
    mount_page(&server).await;
    mount_keyed_file(&server, b"secret body".to_vec()).await;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let output = temp.path().join("out");
    std::fs::create_dir_all(&output).unwrap();
    let config = write_config(&temp, "[history]\nenabled = true\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--config"])
        .arg(config)
        .args([
            "download",
            "--key",
            "EXAMPLE-KEY-0000",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(&output)
        .assert()
        .success();

    let history = std::fs::read_to_string(history_path(&data)).unwrap();
    assert!(history.contains("\"operation\":\"download\""));
    assert!(history.contains("\"result\":\"ok\""));
    assert!(history.contains("example file.bin"));
    assert!(!history.contains("EXAMPLE-KEY-0000"));
    assert!(!history.contains("dlkey"));
}

#[tokio::test]
async fn history_records_only_selected_download_files() {
    let server = MockServer::start().await;
    mount_matomete_page(&server).await;
    mount_named_file(
        &server,
        "0123abcd-000000example-2",
        "selected.bin",
        b"selected".to_vec(),
    )
    .await;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let output = temp.path().join("out");
    std::fs::create_dir_all(&output).unwrap();
    let config = write_config(&temp, "[history]\nenabled = true\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--config"])
        .arg(config)
        .args([
            "download",
            "--select",
            "2",
            &format!("{}/{FILE_ID}", server.uri()),
            "-o",
        ])
        .arg(&output)
        .assert()
        .success();

    let history = std::fs::read_to_string(history_path(&data)).unwrap();
    assert!(history.contains("selected.bin"));
    assert!(!history.contains("example file.bin"));

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
async fn history_write_failure_warns_without_changing_exit_code() {
    let server = MockServer::start().await;
    mount_page(&server).await;
    mount_file(&server, b"hello".to_vec()).await;
    let temp = TempDir::new().unwrap();
    let data_file = temp.path().join("data-file");
    std::fs::write(&data_file, b"not a directory").unwrap();
    let output = temp.path().join("out");
    std::fs::create_dir_all(&output).unwrap();
    let config = write_config(&temp, "[history]\nenabled = true\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("RGFILE_TEST_DATA_DIR", &data_file)
        .args(["--config"])
        .arg(config)
        .args(["download", &format!("{}/{FILE_ID}", server.uri()), "-o"])
        .arg(&output)
        .assert()
        .success()
        .stderr(predicate::str::contains("Warning: failed to write history"));

    assert!(output.join("example file.bin").exists());
}

#[tokio::test]
async fn history_upload_delete_key_is_opt_in_only() {
    let server = MockServer::start().await;
    mount_landing(&server).await;
    mount_upload(&server, format!("{}/{FILE_ID}", server.uri())).await;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let file = write_file(&temp, "upload.bin", b"hello upload");
    let default_config = write_config(&temp, "[history]\nenabled = true\n");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--config"])
        .arg(default_config)
        .args(["upload", "--no-verify"])
        .arg(&file)
        .assert()
        .success();

    let history = std::fs::read_to_string(history_path(&data)).unwrap();
    assert!(!history.contains("EXAMPLE-DELKEY-0000"));
    assert!(!history.contains("delete_key"));

    let opt_in_config = write_config(
        &temp,
        "[history]\nenabled = true\nstore_delete_keys = true\n",
    );
    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("GFILE_TEST_ENTRY_URL", server.uri())
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--config"])
        .arg(opt_in_config)
        .args(["upload", "--no-verify"])
        .arg(&file)
        .assert()
        .success();

    let history = std::fs::read_to_string(history_path(&data)).unwrap();
    assert!(history.contains("EXAMPLE-DELKEY-0000"));
    assert!(history.contains("delete_key"));
}

#[test]
fn history_list_json_and_clear_use_test_data_dir() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let path = history_path(&data);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        concat!(
            "{\"timestamp\":\"2026-07-03T00:00:00Z\",\"operation\":\"download\",\"page_url\":\"https://23.gigafile.nu/0123abcd-000000example\",\"files\":[\"old.bin\"],\"bytes\":1,\"result\":\"ok\"}\n",
            "{\"timestamp\":\"2026-07-03T00:00:01Z\",\"operation\":\"upload\",\"page_url\":\"https://23.gigafile.nu/0123abcd-000000example\",\"files\":[\"new.bin\"],\"bytes\":2,\"result\":\"ok\"}\n",
        ),
    )
    .unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--no-config", "history", "list", "--json", "-n", "1"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    assert_eq!(value["entries"][0]["files"][0], "new.bin");

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--no-config", "history", "clear"])
        .assert()
        .success();
    assert!(!path.exists());
}

async fn mount_page(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/single_basic.html"), "text/html"),
        )
        .mount(server)
        .await;
}

async fn mount_matomete_page(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/matomete_two_files.html"),
            "text/html",
        ))
        .mount(server)
        .await;
}

async fn mount_page_with_user_agent(server: &MockServer, user_agent: &'static str) {
    Mock::given(method("GET"))
        .and(path(format!("/{FILE_ID}")))
        .and(header("user-agent", user_agent))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/single_basic.html"), "text/html"),
        )
        .mount(server)
        .await;
}

async fn mount_file(server: &MockServer, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .insert_header("Content-Type", "application/octet-stream")
                .set_body_bytes(body),
        )
        .mount(server)
        .await;
}

async fn mount_file_with_user_agent(server: &MockServer, user_agent: &'static str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .and(header("user-agent", user_agent))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .insert_header("Content-Type", "application/octet-stream")
                .set_body_bytes(body),
        )
        .mount(server)
        .await;
}

async fn mount_keyed_file(server: &MockServer, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path("/download.php"))
        .and(query_param("file", FILE_ID))
        .and(query_param("dlkey", "EXAMPLE-KEY-0000"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .insert_header("Content-Type", "application/octet-stream")
                .set_body_bytes(body),
        )
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

async fn mount_upload(server: &MockServer, url: String) {
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |_request: &Request| {
            ResponseTemplate::new(200).set_body_json(upload_response(&url))
        })
        .mount(server)
        .await;
}

async fn mount_upload_collecting_lifetimes(
    server: &MockServer,
    url: String,
) -> Arc<Mutex<Vec<String>>> {
    let lifetimes = Arc::new(Mutex::new(Vec::new()));
    let responder_lifetimes = Arc::clone(&lifetimes);
    Mock::given(method("POST"))
        .and(path("/upload_chunk.php"))
        .respond_with(move |request: &Request| {
            responder_lifetimes
                .lock()
                .unwrap()
                .push(multipart_text_field(&request.body, "lifetime"));
            ResponseTemplate::new(200).set_body_json(upload_response(&url))
        })
        .mount(server)
        .await;
    lifetimes
}

fn upload_response(url: &str) -> Value {
    serde_json::json!({
        "status": 0,
        "url": url,
        "delkey": "EXAMPLE-DELKEY-0000",
        "filename": "example file.bin"
    })
}

fn multipart_text_field(body: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(body);
    let marker = format!("name=\"{name}\"\r\n\r\n");
    let start = text.find(&marker).expect("multipart field marker") + marker.len();
    let end = text[start..].find("\r\n--").expect("multipart field end") + start;
    text[start..end].to_owned()
}

fn write_config(temp: &TempDir, body: &str) -> PathBuf {
    let path = temp
        .path()
        .join(format!("config-{}.toml", uuid_like_suffix(body)));
    std::fs::write(&path, body).unwrap();
    path
}

fn write_file(temp: &TempDir, name: &str, body: &[u8]) -> PathBuf {
    let path = temp.path().join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("rgfile").join("history.jsonl")
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn uuid_like_suffix(value: &str) -> u64 {
    value.bytes().fold(0_u64, |hash, byte| {
        hash.wrapping_mul(131).wrapping_add(u64::from(byte))
    })
}
