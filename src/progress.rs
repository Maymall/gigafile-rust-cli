// SPDX-License-Identifier: GPL-3.0-only

use std::io::IsTerminal;

use indicatif::{ProgressBar, ProgressStyle};

#[derive(Clone)]
pub struct ByteProgress {
    bar: Option<ProgressBar>,
}

impl ByteProgress {
    pub fn new(total: Option<u64>, quiet: bool, label: &str) -> Self {
        if quiet || !std::io::stderr().is_terminal() {
            return Self { bar: None };
        }

        let Some(total) = total else {
            return Self { bar: None };
        };

        let bar = ProgressBar::new(total);
        let style = ProgressStyle::with_template(
            "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ");
        bar.set_style(style);
        bar.set_message(label.to_owned());

        Self { bar: Some(bar) }
    }

    pub fn inc(&self, bytes: u64) {
        if let Some(bar) = &self.bar {
            bar.inc(bytes);
        }
    }

    pub fn set_position(&self, bytes: u64) {
        if let Some(bar) = &self.bar {
            bar.set_position(bytes);
        }
    }

    pub fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}
