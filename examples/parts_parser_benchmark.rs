// SPDX-License-Identifier: MIT

//! Standalone microbenchmark for partial-download discovery and HTML parsing.
//!
//! Build it with `benchmarks/run_parts_parser.sh build`. It uses only the
//! production crate and does not add a benchmarking dependency.

use std::{
    env,
    fs::{self, File},
    hint::black_box,
    path::Path,
    process::ExitCode,
    time::Instant,
};

use rgfile::{parser::download::parse_download_page, parts};

const FILE_ID: &str = "0123abcd-000000example";
const SINGLE_HTML: &str = include_str!("../tests/fixtures/single_basic.html");

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("prepare-parts") => {
            let dir = args.next().ok_or_else(usage)?;
            let groups = parse_arg::<usize>(args.next(), "group count")?;
            require_no_more_args(args)?;
            prepare_parts(Path::new(&dir), groups)
        }
        Some("parts") => {
            let dir = args.next().ok_or_else(usage)?;
            let samples = parse_arg::<usize>(args.next(), "sample count")?;
            let iterations = parse_arg::<usize>(args.next(), "iterations per sample")?;
            require_no_more_args(args)?;
            benchmark_parts(Path::new(&dir), samples, iterations)
        }
        Some("parser-single") => {
            let samples = parse_arg::<usize>(args.next(), "sample count")?;
            let iterations = parse_arg::<usize>(args.next(), "iterations per sample")?;
            require_no_more_args(args)?;
            benchmark_parser(SINGLE_HTML.to_owned(), 1, samples, iterations, "single")
        }
        Some("parser-matomete") => {
            let files = parse_arg::<usize>(args.next(), "file count")?;
            let samples = parse_arg::<usize>(args.next(), "sample count")?;
            let iterations = parse_arg::<usize>(args.next(), "iterations per sample")?;
            require_no_more_args(args)?;
            benchmark_parser(
                matomete_fixture(files),
                files,
                samples,
                iterations,
                "matomete",
            )
        }
        _ => Err(usage()),
    }
}

fn prepare_parts(dir: &Path, groups: usize) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    if fs::read_dir(dir)
        .map_err(|error| format!("read {}: {error}", dir.display()))?
        .next()
        .is_some()
    {
        return Err(format!(
            "benchmark fixture directory must be empty: {}",
            dir.display()
        ));
    }

    for index in 0..groups {
        let target = dir.join(format!("fixture-{index:05}.bin"));
        let part = target.with_file_name(format!("fixture-{index:05}.bin.part"));
        let sidecar = target.with_file_name(format!("fixture-{index:05}.bin.part.json"));
        let lock = target.with_file_name(format!("fixture-{index:05}.bin.part.json.lock"));
        let part_file =
            File::create(&part).map_err(|error| format!("create {}: {error}", part.display()))?;

        let sidecar_json = if index % 2 == 0 {
            part_file
                .set_len(512)
                .map_err(|error| format!("resize {}: {error}", part.display()))?;
            format!(r#"{{"version":1,"file_id":"{FILE_ID}","expected":1024,"key_used":false}}"#)
        } else {
            part_file
                .set_len(1024)
                .map_err(|error| format!("resize {}: {error}", part.display()))?;
            format!(
                concat!(
                    r#"{{"version":2,"file_id":"{}","expected":1024,"key_used":false,"segments":["#,
                    r#"{{"start":0,"end":255,"done":true,"downloaded":256}},"#,
                    r#"{{"start":256,"end":511,"done":true,"downloaded":256}},"#,
                    r#"{{"start":512,"end":767,"done":false,"downloaded":0}},"#,
                    r#"{{"start":768,"end":1023,"done":false,"downloaded":0}}"#,
                    r#"]}}"#
                ),
                FILE_ID
            )
        };
        fs::write(&sidecar, sidecar_json)
            .map_err(|error| format!("write {}: {error}", sidecar.display()))?;
        File::create(&lock).map_err(|error| format!("create {}: {error}", lock.display()))?;
    }

    println!("prepared_parts groups={groups} dir={}", dir.display());
    Ok(())
}

fn benchmark_parts(dir: &Path, samples: usize, iterations_per_sample: usize) -> Result<(), String> {
    validate_counts(samples, iterations_per_sample)?;
    let expected_groups = fs::read_dir(dir)
        .map_err(|error| format!("read {}: {error}", dir.display()))?
        .count()
        / 3;
    let warmup = parts::list(dir.to_owned()).map_err(|error| error.user_message())?;
    if warmup.groups.len() != expected_groups {
        return Err(format!(
            "fixture mismatch: expected {expected_groups} groups, got {}",
            warmup.groups.len()
        ));
    }
    black_box(warmup);

    let mut elapsed = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        for _ in 0..iterations_per_sample {
            let report = parts::list(dir.to_owned()).map_err(|error| error.user_message())?;
            black_box(report);
        }
        elapsed.push(started.elapsed().as_nanos() / iterations_per_sample as u128);
    }
    print_summary("parts", expected_groups, &elapsed);
    Ok(())
}

fn benchmark_parser(
    html: String,
    files: usize,
    samples: usize,
    iterations_per_sample: usize,
    case: &str,
) -> Result<(), String> {
    validate_counts(samples, iterations_per_sample)?;
    let warmup = parse_download_page(&html, FILE_ID).map_err(|error| error.user_message())?;
    if warmup.files.len() != files {
        return Err(format!(
            "fixture mismatch: expected {files} files, got {}",
            warmup.files.len()
        ));
    }
    black_box(warmup);

    let mut elapsed = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        for _ in 0..iterations_per_sample {
            let page = parse_download_page(black_box(&html), FILE_ID)
                .map_err(|error| error.user_message())?;
            black_box(page);
        }
        elapsed.push(started.elapsed().as_nanos() / iterations_per_sample as u128);
    }
    print_summary(case, files, &elapsed);
    Ok(())
}

fn matomete_fixture(files: usize) -> String {
    let mut html = String::with_capacity(files.saturating_mul(420));
    html.push_str("<!doctype html><html><body><div id=\"contents_matomete\">");
    for index in 0..files {
        html.push_str(&format!(
            concat!(
                "<div class=\"matomete_file\"><div class=\"matomete_file_info\">",
                "<span>scan</span><span>fixture-{:05}.bin</span>",
                "<span>（10KB）</span><span>expiry</span></div>",
                "<button class=\"download_panel_btn_dl\" ",
                "onclick=\"download({}, '{}-{:05}', false, false);\">",
                "download</button></div>"
            ),
            index, index, FILE_ID, index
        ));
    }
    html.push_str("</div></body></html>");
    html
}

fn print_summary(case: &str, items: usize, samples: &[u128]) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median_ns = median(&sorted);
    let mut deviations = sorted
        .iter()
        .map(|sample| sample.abs_diff(median_ns))
        .collect::<Vec<_>>();
    deviations.sort_unstable();
    let mad = median(&deviations);
    println!(
        "case={case} items={items} samples={} median_ns={median_ns} mad_ns={mad} min_ns={} max_ns={}",
        samples.len(),
        sorted[0],
        sorted[sorted.len() - 1]
    );
}

fn median(sorted: &[u128]) -> u128 {
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2
    } else {
        sorted[middle]
    }
}

fn validate_counts(samples: usize, iterations: usize) -> Result<(), String> {
    if samples == 0 || iterations == 0 {
        Err("sample and iteration counts must be greater than zero".to_owned())
    } else {
        Ok(())
    }
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

fn require_no_more_args(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    if args.next().is_some() {
        Err(usage())
    } else {
        Ok(())
    }
}

fn usage() -> String {
    concat!(
        "usage:\n",
        "  parts-parser-bench prepare-parts <directory> <groups>\n",
        "  parts-parser-bench parts <directory> <samples> <iterations-per-sample>\n",
        "  parts-parser-bench parser-single <samples> <iterations-per-sample>\n",
        "  parts-parser-bench parser-matomete <files> <samples> <iterations-per-sample>"
    )
    .to_owned()
}
