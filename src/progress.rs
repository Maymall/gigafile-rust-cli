// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io::IsTerminal,
    sync::{Arc, Mutex},
};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

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

#[derive(Debug, Clone, Copy)]
pub struct SegmentProgressSpec {
    pub len: u64,
    pub initial: u64,
}

#[derive(Clone)]
pub struct SegmentedProgress {
    inner: Arc<SegmentedProgressInner>,
}

struct SegmentedProgressInner {
    bars: Option<SegmentedBars>,
    positions: Mutex<SegmentPositions>,
    total: Option<u64>,
    segment_totals: Vec<u64>,
}

struct SegmentedBars {
    _multi: MultiProgress,
    main: ProgressBar,
    segments: Vec<ProgressBar>,
}

struct SegmentPositions {
    main: u64,
    segments: Vec<u64>,
}

impl SegmentedProgress {
    pub fn new(
        total: Option<u64>,
        quiet: bool,
        label: &str,
        segments: &[SegmentProgressSpec],
    ) -> Self {
        Self::new_with_options(total, quiet, label, segments, "conn", true)
    }

    pub fn new_with_segment_label(
        total: Option<u64>,
        quiet: bool,
        label: &str,
        segments: &[SegmentProgressSpec],
        segment_label: &str,
    ) -> Self {
        Self::new_with_options(total, quiet, label, segments, segment_label, false)
    }

    fn new_with_options(
        total: Option<u64>,
        quiet: bool,
        label: &str,
        segments: &[SegmentProgressSpec],
        segment_label: &str,
        segment_speed: bool,
    ) -> Self {
        let segment_totals = segments
            .iter()
            .map(|segment| segment.len)
            .collect::<Vec<_>>();
        let segment_positions = segments
            .iter()
            .zip(segment_totals.iter())
            .map(|(segment, total)| segment.initial.min(*total))
            .collect::<Vec<_>>();
        let main_position = segment_positions.iter().sum::<u64>();
        let bars = if quiet || !std::io::stderr().is_terminal() {
            None
        } else {
            total.map(|total| {
                segmented_bars(
                    total,
                    label,
                    &segment_totals,
                    &segment_positions,
                    segment_label,
                    segment_speed,
                )
            })
        };

        Self {
            inner: Arc::new(SegmentedProgressInner {
                bars,
                positions: Mutex::new(SegmentPositions {
                    main: main_position,
                    segments: segment_positions,
                }),
                total,
                segment_totals,
            }),
        }
    }

    pub fn inc(&self, index: usize, bytes: u64) {
        let Some(segment_total) = self.inner.segment_totals.get(index).copied() else {
            debug_assert!(false, "missing segmented progress index {index}");
            return;
        };
        let mut positions = self
            .inner
            .positions
            .lock()
            .expect("segmented progress mutex poisoned");
        let Some(segment_position) = positions.segments.get_mut(index) else {
            debug_assert!(false, "missing segmented progress index {index}");
            return;
        };
        let before = *segment_position;
        *segment_position = segment_position.saturating_add(bytes).min(segment_total);
        let advanced = *segment_position - before;
        positions.main = positions
            .main
            .saturating_add(advanced)
            .min(self.inner.total.unwrap_or(u64::MAX));
        drop(positions);

        if let Some(bars) = &self.inner.bars {
            bars.main.inc(advanced);
            if let Some(bar) = bars.segments.get(index) {
                bar.inc(advanced);
            }
        }
    }

    pub fn set_segment_position(&self, index: usize, bytes: u64) {
        let Some(segment_total) = self.inner.segment_totals.get(index).copied() else {
            debug_assert!(false, "missing segmented progress index {index}");
            return;
        };
        let mut positions = self
            .inner
            .positions
            .lock()
            .expect("segmented progress mutex poisoned");
        let position = bytes.min(segment_total);
        let Some(segment_position) = positions.segments.get_mut(index) else {
            debug_assert!(false, "missing segmented progress index {index}");
            return;
        };
        *segment_position = position;
        positions.main = positions
            .segments
            .iter()
            .sum::<u64>()
            .min(self.inner.total.unwrap_or(u64::MAX));
        let main = positions.main;
        drop(positions);

        if let Some(bars) = &self.inner.bars {
            bars.main.set_position(main);
            if let Some(bar) = bars.segments.get(index) {
                bar.set_position(position);
            }
        }
    }

    pub fn finish(&self) {
        if let Some(bars) = &self.inner.bars {
            for bar in &bars.segments {
                bar.finish_and_clear();
            }
            bars.main.finish_and_clear();
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> (u64, Vec<u64>) {
        let positions = self
            .inner
            .positions
            .lock()
            .expect("segmented progress mutex poisoned");
        (positions.main, positions.segments.clone())
    }
}

fn segmented_bars(
    total: u64,
    label: &str,
    segment_totals: &[u64],
    segment_positions: &[u64],
    segment_label: &str,
    segment_speed: bool,
) -> SegmentedBars {
    let multi = MultiProgress::new();
    let main = multi.add(
        ProgressBar::new(total)
            .with_style(main_style())
            .with_message(label.to_owned()),
    );
    main.set_position(segment_positions.iter().sum::<u64>().min(total));
    main.tick();

    let mut segments = Vec::with_capacity(segment_totals.len());
    let mut previous = main.clone();
    for (index, total) in segment_totals.iter().copied().enumerate() {
        let prefix = if index + 1 == segment_totals.len() {
            "  └─"
        } else {
            "  ├─"
        };
        let bar = multi.insert_after(
            &previous,
            ProgressBar::new(total)
                .with_style(segment_style(segment_speed))
                .with_prefix(prefix.to_owned())
                .with_message(format!("{segment_label} {}", index + 1)),
        );
        if let Some(position) = segment_positions.get(index).copied() {
            bar.set_position(position.min(total));
        }
        bar.tick();
        previous = bar.clone();
        segments.push(bar);
    }

    SegmentedBars {
        _multi: multi,
        main,
        segments,
    }
}

fn main_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta_precise}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("=> ")
}

fn segment_style(show_speed: bool) -> ProgressStyle {
    let template = if show_speed {
        "{prefix} {msg} [{bar:24.white/blue}] {bytes}/{total_bytes} {bytes_per_sec}"
    } else {
        "{prefix} {msg} [{bar:24.white/blue}] {bytes}/{total_bytes}"
    };
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmented_progress_initializes_each_segment_position() {
        let progress = SegmentedProgress::new(
            Some(300),
            true,
            "example.bin",
            &[
                SegmentProgressSpec {
                    len: 100,
                    initial: 100,
                },
                SegmentProgressSpec {
                    len: 100,
                    initial: 40,
                },
                SegmentProgressSpec {
                    len: 100,
                    initial: 0,
                },
            ],
        );

        assert_eq!(progress.snapshot(), (140, vec![100, 40, 0]));
    }

    #[test]
    fn segmented_progress_increment_updates_main_and_one_segment() {
        let progress = SegmentedProgress::new(
            Some(20),
            true,
            "example.bin",
            &[
                SegmentProgressSpec {
                    len: 10,
                    initial: 2,
                },
                SegmentProgressSpec {
                    len: 10,
                    initial: 4,
                },
            ],
        );

        progress.inc(0, 3);
        progress.inc(1, 99);

        assert_eq!(progress.snapshot(), (15, vec![5, 10]));
    }

    #[test]
    fn segmented_progress_set_segment_position_recomputes_main() {
        let progress = SegmentedProgress::new(
            Some(30),
            true,
            "example.bin",
            &[
                SegmentProgressSpec {
                    len: 10,
                    initial: 5,
                },
                SegmentProgressSpec {
                    len: 20,
                    initial: 5,
                },
            ],
        );

        progress.set_segment_position(1, 12);

        assert_eq!(progress.snapshot(), (17, vec![5, 12]));
    }
}
