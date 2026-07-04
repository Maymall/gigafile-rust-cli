// SPDX-License-Identifier: MIT

use std::{
    fs::OpenOptions,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use assert_cmd::prelude::*;
use fs2::FileExt;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

const FILE_ID: &str = "0123abcd-000000example";
const DELKEY: &str = "EXA1";

#[tokio::test]
async fn cli_delete_yes_with_delkey_succeeds() {
    let server = MockServer::start().await;
    mount_delete(&server, 0).await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "delete",
            "--yes",
            "--delkey",
            DELKEY,
            &format!("{}/{FILE_ID}", server.uri()),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted"));
}

#[tokio::test]
async fn cli_delete_wrong_delkey_exits_22() {
    let server = MockServer::start().await;
    mount_delete(&server, 1).await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "delete",
            "--yes",
            "--delkey",
            DELKEY,
            &format!("{}/{FILE_ID}", server.uri()),
        ])
        .assert()
        .code(22)
        .stderr(predicate::str::contains("delete status 1"));
}

#[test]
fn cli_delete_without_delkey_or_history_exits_2() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "delete",
            "--yes",
            "https://23.gigafile.nu/0123abcd-000000example",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("delete key required"));
}

#[test]
fn cli_delete_without_yes_requires_tty() {
    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "delete",
            "--delkey",
            DELKEY,
            "https://23.gigafile.nu/0123abcd-000000example",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("pass --yes"));
}

#[tokio::test]
async fn cli_delete_uses_history_delete_key_and_records_delete() {
    let server = MockServer::start().await;
    mount_delete(&server, 0).await;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let history_path = data.join("rgfile").join("history.jsonl");
    std::fs::create_dir_all(history_path.parent().unwrap()).unwrap();
    let url = format!("{}/{FILE_ID}", server.uri());
    std::fs::write(
        &history_path,
        format!(
            "{{\"timestamp\":\"2026-07-04T00:00:00Z\",\"operation\":\"upload\",\"page_url\":\"{url}\",\"files\":[\"example file.bin\"],\"bytes\":24,\"result\":\"ok\",\"delete_key\":\"{DELKEY}\"}}\n"
        ),
    )
    .unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(
        &config,
        "[history]\nenabled = true\nstore_delete_keys = true\n",
    )
    .unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .env("RGFILE_TEST_DATA_DIR", &data)
        .args(["--config"])
        .arg(&config)
        .args(["delete", "--yes", &url])
        .assert()
        .success();

    let history = std::fs::read_to_string(history_path).unwrap();
    assert!(history.contains("\"operation\":\"delete\""));
    assert!(history.contains("\"result\":\"ok\""));
}

#[tokio::test]
async fn cli_delete_verbose_retry_redacts_delkey() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_response = attempts.clone();
    Mock::given(method("GET"))
        .and(path("/remove.php"))
        .and(query_param("file", FILE_ID))
        .and(query_param("delkey", DELKEY))
        .respond_with(move |_request: &Request| {
            if attempts_for_response.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": 0}))
            }
        })
        .mount(&server)
        .await;

    Command::cargo_bin("rgfile")
        .unwrap()
        .env("GFILE_TEST_ALLOW_ANY_HOST", "1")
        .args([
            "--no-config",
            "-vv",
            "delete",
            "--yes",
            "--retries",
            "1",
            "--delkey",
            DELKEY,
            &format!("{}/{FILE_ID}", server.uri()),
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("delkey=***"))
        .stderr(predicate::str::contains(DELKEY).not());
}

#[test]
fn cli_parts_list_json_reports_groups() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("example.bin.part"), vec![0_u8; 10]).unwrap();
    std::fs::write(
        temp.path().join("example.bin.part.json"),
        serde_json::json!({
            "version": 1,
            "file_id": FILE_ID,
            "expected": 20,
            "key_used": false
        })
        .to_string(),
    )
    .unwrap();

    let output = Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--no-config", "parts", "list"])
        .arg(temp.path())
        .arg("--json")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["groups"][0]["target_name"], "example.bin");
    assert_eq!(value["groups"][0]["completed_bytes"], 10);
    assert_eq!(value["groups"][0]["expected_bytes"], 20);
}

#[test]
fn cli_parts_clean_without_yes_requires_tty() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("example.bin.part"), b"partial").unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--no-config", "parts", "clean"])
        .arg(temp.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("pass --yes"));
}

#[test]
fn cli_parts_clean_yes_skips_active_lock_and_deletes_stale() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("active.bin.part"), b"active").unwrap();
    let active_lock = temp.path().join("active.bin.part.json.lock");
    std::fs::write(&active_lock, b"").unwrap();
    std::fs::write(temp.path().join("stale.bin.part"), b"stale").unwrap();
    std::fs::write(temp.path().join("stale.bin.part.json"), b"{}").unwrap();

    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&active_lock)
        .unwrap();
    FileExt::try_lock_exclusive(&lock_file).unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--no-config", "parts", "clean", "--yes"])
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("skipped active"));

    assert!(temp.path().join("active.bin.part").exists());
    assert!(!temp.path().join("stale.bin.part").exists());
    assert!(!temp.path().join("stale.bin.part.json").exists());

    FileExt::unlock(&lock_file).unwrap();
}

#[test]
fn cli_parts_clean_yes_deletes_sidecar_tmp() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("stale.bin.part"), b"stale").unwrap();
    std::fs::write(temp.path().join("stale.bin.part.json.tmp"), b"tmp").unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .args(["--no-config", "parts", "clean", "--yes"])
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted\tstale.bin"));

    assert!(!temp.path().join("stale.bin.part").exists());
    assert!(!temp.path().join("stale.bin.part.json.tmp").exists());
}

#[test]
fn cli_parts_clean_older_than_filter_keeps_new_group() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("new.bin.part"), b"new").unwrap();

    Command::cargo_bin("rgfile")
        .unwrap()
        .args([
            "--no-config",
            "parts",
            "clean",
            "--older-than",
            "9999",
            "--yes",
        ])
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to clean"));

    assert!(temp.path().join("new.bin.part").exists());
}

async fn mount_delete(server: &MockServer, status: i64) {
    Mock::given(method("GET"))
        .and(path("/remove.php"))
        .and(query_param("file", FILE_ID))
        .and(query_param("delkey", DELKEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": status
        })))
        .mount(server)
        .await;
}
