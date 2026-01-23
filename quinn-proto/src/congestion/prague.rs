use std::any::Any;
use std::sync::Arc;

use super::{BASE_DATAGRAM_SIZE, Controller, ControllerFactory};
use crate::Instant;
use crate::connection::RttEstimator;

/// A scalable congestion controller
#[derive(Debug, Clone)]
pub struct Prague {
    config: Arc<PragueConfig>,
    current_mtu: u64,
    /// Maximum number of bytes in flight that may be sent.
    window: u64,
    /// Slow start threshold in bytes. When the congestion window is below ssthresh, the mode is
    /// slow start and the window grows by the number of bytes acknowledged.
    ssthresh: u64,
    /// The time when QUIC first detects a loss, causing it to enter recovery. When a packet sent
    /// after this time is acknowledged, QUIC exits recovery.
    recovery_start_time: Instant,
    /// Bytes which had been acked by the peer since leaving slow start
    bytes_acked: u64,
    /// History of last n segments arriving Used for calculating the packet
    /// marking probability.
    bytes_marked: u64,
    alpha: f32,
}

impl Prague {
    /// Construct a state using the given `config` and current time `now`
    pub fn new(config: Arc<PragueConfig>, now: Instant, current_mtu: u16) -> Self {
        Self {
            window: config.initial_window,
            ssthresh: u64::MAX,
            recovery_start_time: now,
            current_mtu: current_mtu as u64,
            config,
            bytes_acked: 0,
            bytes_marked: 0,
            alpha: 1.0,
        }
    }

    fn minimum_window(&self) -> u64 {
        2 * self.current_mtu
    }

    fn update_marking_fraction(&mut self) {
        let frac = self.bytes_marked as f32 / self.bytes_acked as f32;
        self.alpha += self.config.g * (frac - self.alpha);
    }
}

impl Controller for Prague {
    fn on_ack(
        &mut self,
        _now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        _rtt: &RttEstimator,
    ) {
        if app_limited || sent <= self.recovery_start_time {
            return;
        }

        if self.window < self.ssthresh {
            // Slow start
            self.window += bytes;

            if self.bytes_marked > 0 || self.window >= self.ssthresh {
                // Exiting slow start
                // Initialize `bytes_acked` for congestion avoidance. The idea
                // here is that any bytes over `sshthresh` will already be counted
                // towards the congestion avoidance phase - independent of when
                // how close to `sshthresh` the `window` was when switching states,
                // and independent of datagram sizes.
                self.bytes_acked = self.window - self.ssthresh;
                self.bytes_marked = self.bytes_marked.saturating_sub(self.ssthresh);
                self.update_marking_fraction();
            }
        } else {
            // Congestion avoidance
            // This implementation uses the method which does not require
            // floating point math, which also increases the window by 1 datagram
            // for every round trip.
            // This mechanism is called Appropriate Byte Counting in
            // https://tools.ietf.org/html/rfc3465
            self.bytes_acked += bytes;

            if self.bytes_acked >= self.window {
                self.bytes_acked -= self.window;
                self.bytes_marked = self.bytes_marked.saturating_sub(self.window);
                self.update_marking_fraction();
                self.window += self.current_mtu;
            }
        }
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        bytes_affected: u64,
    ) {
        self.bytes_marked += bytes_affected;

        if sent <= self.recovery_start_time {
            return;
        }

        self.recovery_start_time = now;
        self.window = (self.window as f32 * self.alpha) as u64;
        self.window = self.window.max(self.minimum_window());
        self.ssthresh = ((1.0 - self.alpha / 2.0) * self.window as f32) as u64;

        if is_persistent_congestion {
            self.window = self.minimum_window();
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu as u64;
        self.window = self.window.max(self.minimum_window());
    }

    fn window(&self) -> u64 {
        self.window
    }

    fn metrics(&self) -> super::ControllerMetrics {
        super::ControllerMetrics {
            congestion_window: self.window(),
            ssthresh: Some(self.ssthresh),
            pacing_rate: None,
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_window
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Configuration for the `Prague` congestion controller
#[derive(Debug, Clone)]
pub struct PragueConfig {
    initial_window: u64,
    g: f32,
}

impl PragueConfig {
    /// Default limit on the amount of outstanding data in bytes.
    ///
    /// Recommended value: `min(10 * max_datagram_size, max(2 * max_datagram_size, 14720))`
    pub fn initial_window(&mut self, value: u64) -> &mut Self {
        self.initial_window = value;
        self
    }
}

impl Default for PragueConfig {
    fn default() -> Self {
        Self {
            initial_window: 14720.clamp(2 * BASE_DATAGRAM_SIZE, 10 * BASE_DATAGRAM_SIZE),
            g: 0.0625,
        }
    }
}

impl ControllerFactory for PragueConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Prague::new(self, now, current_mtu))
    }
}
