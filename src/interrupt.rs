// SPDX-License-Identifier: MIT

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use indicatif::HumanBytes;

use crate::download;

/// The transfer whose partial state should be reported when the process is
/// interrupted with Ctrl-C. Updated by the download path as transfers start
/// and finish; read once from the signal handler.
static ACTIVE_DOWNLOAD: Mutex<Option<ActiveDownload>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub struct ActiveDownload {
    pub part_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub expected: Option<u64>,
}

pub fn set_active_download(active: Option<ActiveDownload>) {
    if let Ok(mut guard) = ACTIVE_DOWNLOAD.lock() {
        *guard = active;
    }
}

/// Install a Ctrl-C watcher that prints how much of the current download made
/// it to disk and where the `.part` file lives, then exits with the
/// conventional 130 code. Progress is read back from the sidecar/`.part` on
/// disk at interrupt time, so the summary matches what a later resume can use.
pub fn spawn_ctrl_c_reporter() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Handler could not be installed; leave default SIGINT behavior.
            return;
        }
        let active = ACTIVE_DOWNLOAD.lock().ok().and_then(|guard| guard.clone());
        eprintln!();
        match active {
            Some(active) => {
                match download::bytes_completed_on_disk(&active.part_path, &active.sidecar_path) {
                    Some(downloaded) => eprintln!(
                        "{}",
                        format_interrupt_summary(downloaded, active.expected, &active.part_path)
                    ),
                    // Progress could not be read back reliably; say so rather
                    // than guessing, but still point at the kept .part.
                    None => eprintln!(
                        "Interrupted.\nPartial download kept: {}\nRe-run the same command to resume.",
                        active.part_path.display()
                    ),
                }
            }
            None => eprintln!("Interrupted."),
        }
        std::process::exit(130);
    });
}

fn format_interrupt_summary(downloaded: u64, expected: Option<u64>, part_path: &Path) -> String {
    let first_line = match expected.filter(|expected| *expected > 0) {
        Some(expected) => format!(
            "Interrupted at {:.1}% ({} / {}).",
            downloaded as f64 / expected as f64 * 100.0,
            HumanBytes(downloaded),
            HumanBytes(expected)
        ),
        None => format!("Interrupted after {}.", HumanBytes(downloaded)),
    };
    format!(
        "{first_line}\nPartial download kept: {}\nRe-run the same command to resume.",
        part_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_with_expected_total_shows_percent() {
        let summary =
            format_interrupt_summary(512, Some(2048), Path::new("/downloads/example.bin.part"));

        assert!(summary.contains("Interrupted at 25.0% (512 B / 2.00 KiB)."));
        assert!(summary.contains("Partial download kept: /downloads/example.bin.part"));
        assert!(summary.contains("Re-run the same command to resume."));
    }

    #[test]
    fn summary_without_expected_total_shows_bytes_only() {
        let summary = format_interrupt_summary(1024, None, Path::new("example.bin.part"));

        assert!(summary.contains("Interrupted after 1.00 KiB."));
        assert!(!summary.contains('%'));
    }

    #[test]
    fn summary_with_zero_expected_avoids_division() {
        let summary = format_interrupt_summary(0, Some(0), Path::new("example.bin.part"));

        assert!(summary.contains("Interrupted after 0 B."));
    }
}
