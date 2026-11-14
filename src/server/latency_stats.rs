//! Host-side latency measurement of the capture loop. Measurement and logging only.
//! What each number means: docs/LATENCY-METRICS.md.

use crate::client::latency::{format_summary, Samples, REPORT_INTERVAL};
use base::config::keys;
use hbb_common::{config::Config, log};
use std::time::{Duration, Instant};

pub(super) struct LatencyStats {
    display: usize,
    enabled: bool,
    window_start: Instant,
    sent: usize,
    skipped: usize,
    idle: usize,
    grab: Samples,
    encode: Samples,
    wait: Samples,
    sleep: Samples,
}

impl LatencyStats {
    pub(super) fn new(display: usize) -> Self {
        Self {
            display,
            enabled: is_enabled(),
            window_start: Instant::now(),
            sent: 0,
            skipped: 0,
            idle: 0,
            grab: Default::default(),
            encode: Default::default(),
            wait: Default::default(),
            sleep: Default::default(),
        }
    }

    /// `grab`: capturer call until it returned the frame.
    /// `encode`: from there until the encoded frame was queued to the connections.
    pub(super) fn on_frame(&mut self, grab: Duration, encode: Duration, sent: bool) {
        if !self.enabled {
            return;
        }
        self.grab.push(grab);
        self.encode.push(encode);
        if sent {
            self.sent += 1;
        } else {
            self.skipped += 1;
        }
    }

    /// The capturer had no new image within one frame period.
    pub(super) fn on_idle(&mut self) {
        if self.enabled {
            self.idle += 1;
        }
    }

    /// Time until every connection took the frame off its queue to start sending it.
    pub(super) fn on_wait(&mut self, sent: bool, wait: Duration) {
        if self.enabled && sent {
            self.wait.push(wait);
        }
    }

    pub(super) fn on_sleep(&mut self, sleep: Duration) {
        if self.enabled {
            self.sleep.push(sleep);
        }
    }

    pub(super) fn tick(&mut self, spf: Duration) {
        let elapsed = self.window_start.elapsed();
        if elapsed < REPORT_INTERVAL {
            return;
        }
        if self.enabled && self.sent + self.skipped + self.idle > 0 {
            log::info!(
                "LATENCY server display={} fps={:.1} spf={}ms grab={} encode={} wait={} sleep={} sent={} skipped={} idle={}",
                self.display,
                self.sent as f64 / elapsed.as_secs_f64(),
                spf.as_millis(),
                format_summary(self.grab.summary()),
                format_summary(self.encode.summary()),
                format_summary(self.wait.summary()),
                format_summary(self.sleep.summary()),
                self.sent,
                self.skipped,
                self.idle,
            );
        }
        self.window_start = Instant::now();
        self.sent = 0;
        self.skipped = 0;
        self.idle = 0;
        self.grab.clear();
        self.encode.clear();
        self.wait.clear();
        self.sleep.clear();
        self.enabled = is_enabled();
    }
}

fn is_enabled() -> bool {
    Config::get_option(keys::OPTION_ENABLE_LATENCY_LOG) == "Y"
}
