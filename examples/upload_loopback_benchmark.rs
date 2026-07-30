// SPDX-License-Identifier: MIT

//! Reproducible loopback upload benchmark driver.
//!
//! This example deliberately calls the library API so release builds can use a
//! local HTTP endpoint without weakening the production CLI's host checks.

use std::{env, path::PathBuf, process::ExitCode, time::Duration};

use rgfile::upload::{UploadOptions, upload};
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let entry_url = args.next().ok_or_else(usage)?;
    let file = PathBuf::from(args.next().ok_or_else(usage)?);
    let chunk_size = parse_arg::<u64>(args.next(), "chunk size")?;
    let threads = parse_arg::<u8>(args.next(), "thread count")?;
    let iterations = parse_arg::<u32>(args.next(), "iteration count")?;
    if args.next().is_some() || iterations == 0 {
        return Err(usage());
    }

    for iteration in 0..iterations {
        let started = std::time::Instant::now();
        let report = upload(UploadOptions {
            file: file.clone(),
            lifetime: 3,
            chunk_size,
            verify: false,
            timeout: Duration::from_secs(120),
            retries: 0,
            threads,
            user_agent: Some("rgfile-loopback-benchmark".to_owned()),
            dump_page: None,
            quiet: true,
            allow_any_host: true,
            entry_url: entry_url.clone(),
        })
        .await
        .map_err(|error| error.user_message())?;
        let elapsed = started.elapsed();
        println!(
            "{}",
            json!({
                "iteration": iteration,
                "elapsed_ns": elapsed.as_nanos(),
                "bytes": report.bytes,
                "chunk_size": chunk_size,
                "threads": threads,
            })
        );
    }

    Ok(())
}

fn parse_arg<T>(value: Option<String>, label: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .ok_or_else(usage)?
        .parse()
        .map_err(|_| format!("invalid {label}"))
}

fn usage() -> String {
    "usage: upload_loopback_benchmark <entry-url> <file> <chunk-bytes> <threads> <iterations>"
        .to_owned()
}
