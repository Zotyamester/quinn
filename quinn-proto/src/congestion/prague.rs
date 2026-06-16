use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use super::{Controller, ControllerFactory};
use crate::Instant;
use crate::congestion::{Cubic, CubicConfig};
use crate::connection::RttEstimator;
use crate::frame::EcnCounts;

/// A port of the Prague congestion controller to Quinn
#[derive(Debug, Clone)]
pub struct Prague {
    controller: Cubic,
    ect1_enabled: bool,
    ect_count: f64,
    ce_count: f64,
    last_alpha_update: Instant,
    alpha: Option<f64>,
    rtt_virt: Duration,
    last_ecn_reduction: Option<(Instant, u64)>,
    reduce_rtt_dependence: bool,
    connection_start_time: Instant,
}

impl Prague {
    /// Following the Linux Prague reference implementation's choice of constants according to
    /// 2.4.4. Reduced RTT-Dependence (draft-briscoe-iccrg-prague-congestion-control-04)
    const RTT_VIRT_MIN: Duration = Duration::from_millis(25);

    const EWMA_GAIN: f64 = 1.0 / 16.0;

    /// Construct a state using the given `config` and current time `now`
    pub fn new(_config: Arc<PragueConfig>, now: Instant, current_mtu: u16) -> Self {
        let cubic_config = Arc::new(CubicConfig::default());
        let cubic = Cubic::new(cubic_config, now, current_mtu);

        Self {
            controller: cubic,
            ect1_enabled: false,
            ect_count: 0.0,
            ce_count: 0.0,
            last_alpha_update: now,
            alpha: None,
            rtt_virt: Prague::RTT_VIRT_MIN,
            last_ecn_reduction: None,
            reduce_rtt_dependence: false,
            connection_start_time: now,
        }
    }
}

impl Controller for Prague {
    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        // Use a corrected, virtual RTT by adjusting the smoothed RTT to account for long-running
        // classic flows with high RTTs, as recommended in A.1.6. Reduce RTT Dependence (RFC9331)
        self.rtt_virt = rtt.get().max(Prague::RTT_VIRT_MIN);

        if !self.ect1_enabled {
            self.controller.on_ack(now, sent, bytes, app_limited, rtt);
            return;
        }

        let original_cwnd = self.controller.window();
        self.controller.on_ack(now, sent, bytes, app_limited, rtt);

        let new_cwnd = self.controller.window();
        if new_cwnd > original_cwnd {
            // Check if we are in slow start.
            let in_slow_start = self
                .controller
                .metrics()
                .ssthresh
                .map(|ssthresh| original_cwnd < ssthresh)
                .unwrap_or(false);

            if !in_slow_start {
                if !self.reduce_rtt_dependence {
                    self.reduce_rtt_dependence =
                        now.saturating_duration_since(self.connection_start_time) > rtt.get() * 500;
                }

                if self.reduce_rtt_dependence {
                    let srtt = rtt.get();
                    let ratio = srtt.as_secs_f64() / self.rtt_virt.as_secs_f64();
                    let deflator = (ratio * ratio).min(1.0);
                    let cwnd_increase = new_cwnd - original_cwnd;
                    let scaled_increase = (cwnd_increase as f64 * deflator) as u64;
                    self.controller.set_window(original_cwnd + scaled_increase);
                }
            }
        }
    }

    fn on_ecn_delivery(&mut self, now: Instant, increment: EcnCounts) {
        if !self.ect1_enabled || (increment.ect1 == 0 && increment.ce == 0) {
            return; // Not an event that concerns an L4S controller such as Prague
        }
        if let Some(alpha) = self.alpha {
            self.ect_count += increment.ect1 as f64;
            self.ce_count += increment.ce as f64;
            if now.saturating_duration_since(self.last_alpha_update) > self.rtt_virt {
                let frac: f64 = self.ce_count / (self.ce_count + self.ect_count);
                self.alpha = Some((1.0 - Prague::EWMA_GAIN) * alpha + Prague::EWMA_GAIN * frac);
                self.last_alpha_update = now;
                self.ect_count = 0.0;
                self.ce_count = 0.0;
            }
        } else if increment.ce > 0 {
            // Initialize alpha to 1.0 on the first CE mark, per the Prague spec.
            self.alpha = Some(1.0);
            self.last_alpha_update = now;
            self.ect_count = increment.ect1 as f64;
            self.ce_count = increment.ce as f64;
        }
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        is_ecn: bool,
        lost_bytes: u64,
        increment: EcnCounts,
    ) {
        if !self.ect1_enabled {
            self.controller.on_congestion_event(
                now,
                sent,
                is_persistent_congestion,
                is_ecn,
                lost_bytes,
                increment,
            );
            return;
        }

        // Handle Loss (loss-only / ECN+loss).
        if lost_bytes > 0 {
            // Check if we should credit a recent ECN reduction to avoid double-dipping.
            if let Some((last_time, last_reduction_size)) = self.last_ecn_reduction {
                if now.saturating_duration_since(last_time) < self.rtt_virt {
                    // Undo the ECN reduction in the underlying controller
                    let undone_cwnd = self.controller.window() + last_reduction_size;
                    self.controller.set_window(undone_cwnd);
                    self.controller.set_ssthresh(undone_cwnd);
                }
            }
            self.last_ecn_reduction = None; // Do not try to compensate for additional losses that may occur

            // Delegate loss handling to the underlying controller.
            self.controller.on_congestion_event(
                now,
                sent,
                is_persistent_congestion,
                false, // Treat as normal loss event
                lost_bytes,
                EcnCounts::ZERO,
            );
            return;
        }

        // Handle pure ECN congestion event (diff.ce > 0).
        if is_ecn && increment.ce > 0 {
            // Limit ECN window reduction to at most once per virtual RTT.
            // This is necessary to satisfy 2.4.2. Multiplicative Decrease on ECN Feedback (draft-briscoe-iccrg-prague-congestion-control-04) stating
            // > the Prague CC [...] only triggers a multiplicative decrease to its congestion window when
            // > it actually receives an ACK carrying ECN feedback. Then it suppresses any further decreases
            // > for one round trip, even if it receives further ECN feedback."
            let in_cooldown = self
                .last_ecn_reduction
                .map(|(last_time, _)| now.saturating_duration_since(last_time) < self.rtt_virt)
                .unwrap_or(false);

            if !in_cooldown {
                let original_cwnd = self.controller.window();

                // Call the underlying controller's congestion event handler to set up recovery state,
                // update recovery_start_time, reset the cubic/reno epoch, etc.
                self.controller.on_congestion_event(
                    now,
                    sent,
                    is_persistent_congestion,
                    true,
                    0,
                    increment,
                );

                // Find out how much the underlying controller reduced the window.
                let cwnd_reduction = original_cwnd.saturating_sub(self.controller.window());

                // Scale the reduction by alpha.
                let alpha = self.alpha.unwrap_or(0.0);
                let prague_reduction = (cwnd_reduction as f64 * alpha) as u64;

                // Apply the Prague ECN-scaled reduction.
                let new_cwnd = original_cwnd.saturating_sub(prague_reduction);
                self.controller.set_window(new_cwnd);
                self.controller.set_ssthresh(new_cwnd);
                self.controller.exit_recovery(now);

                // Record the reduction details.
                self.last_ecn_reduction = Some((now, prague_reduction));
            }
        }
    }

    fn exit_recovery(&mut self, now: Instant) {
        self.controller.exit_recovery(now);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.controller.on_mtu_update(new_mtu);
    }

    fn set_window(&mut self, size: u64) {
        self.controller.set_window(size);
    }

    fn set_ssthresh(&mut self, size: u64) {
        self.controller.set_ssthresh(size);
    }

    fn window(&self) -> u64 {
        self.controller.window()
    }

    fn metrics(&self) -> super::ControllerMetrics {
        self.controller.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.controller.initial_window()
    }

    fn enable_ect0(&mut self) -> bool {
        self.controller.enable_ect0()
    }

    fn enable_ect1(&mut self) -> bool {
        self.ect1_enabled = true;
        true
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Configuration for the `Prague` congestion controller
#[derive(Debug, Clone)]
pub struct PragueConfig {}

impl PragueConfig {}

impl Default for PragueConfig {
    fn default() -> Self {
        Self {}
    }
}

impl ControllerFactory for PragueConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Prague::new(self, now, current_mtu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::EcnCounts;

    #[test]
    fn test_prague_ecn_accumulation_and_reduction() {
        let now = Instant::now();
        let config = Arc::new(PragueConfig::default());
        let mut prague = Prague::new(config, now, 1200);
        prague.enable_ect1();

        // 1. Initial state: alpha is None, window is initial window
        let initial_cwnd = prague.window();
        assert!(initial_cwnd > 0);

        println!("Before enable_ect1: ect1_enabled = {}", prague.ect1_enabled);
        prague.enable_ect1();
        println!("After enable_ect1: ect1_enabled = {}", prague.ect1_enabled);

        // 2. Process first ECN CE mark.
        // In production, the connection calls both `on_ecn_delivery` and `on_congestion_event`.
        let increment = EcnCounts {
            ect0: 0,
            ect1: 10,
            ce: 1,
        };
        prague.on_ecn_delivery(now, increment);
        println!("After on_ecn_delivery: alpha = {:?}", prague.alpha);
        prague.on_congestion_event(now, now, false, true, 0, increment);

        let cwnd_after_first = prague.window();
        assert_eq!(prague.alpha, Some(1.0));
        assert!(cwnd_after_first < initial_cwnd);

        // Verify that recovery was exited and normal ACK processing is not blocked.
        let rtt = RttEstimator::new(Duration::from_millis(50));
        // Force slow start to make window growth on ACK immediate and large.
        prague.set_ssthresh(u64::MAX);
        prague.on_ack(
            now + Duration::from_millis(10),
            now, // packet sent at `now`
            50000,
            false,
            &rtt,
        );
        assert!(prague.window() > cwnd_after_first);

        // Restore the window and ssthresh for the rest of the test
        prague.set_window(cwnd_after_first);
        prague.set_ssthresh(cwnd_after_first);

        assert!(
            prague
                .last_ecn_reduction
                .is_some_and(|(time, _)| time == now)
        );

        // 3. Process another ECN CE mark within cooldown (within 25ms).
        // Since we are in cooldown, the window should NOT be reduced further.
        let diff_cooldown = EcnCounts {
            ect0: 0,
            ect1: 5,
            ce: 1,
        };
        prague.on_ecn_delivery(now + Duration::from_millis(10), diff_cooldown);
        prague.on_congestion_event(
            now + Duration::from_millis(10),
            now + Duration::from_millis(10),
            false,
            true,
            0,
            diff_cooldown,
        );
        assert_eq!(prague.window(), cwnd_after_first);

        // 4. Test loss credit / double dipping prevention.
        // A packet loss occurs within rtt_virt (25ms) of the ECN reduction (e.g., at now + 15ms).
        // It should undo the ECN reduction and apply the standard Cubic loss reduction.
        prague.on_congestion_event(
            now + Duration::from_millis(15),
            now + Duration::from_millis(15),
            false,
            false,
            1000,
            EcnCounts::ZERO,
        );
        assert_eq!(prague.window(), cwnd_after_first);
        assert_eq!(prague.last_ecn_reduction, None); // Cleared
    }

    #[test]
    fn test_prague_alpha_decay_and_fractional_reduction() {
        let now = Instant::now();
        let config = Arc::new(PragueConfig::default());
        let mut prague = Prague::new(config, now, 1200);
        prague.enable_ect1();

        // 1. First ECN mark initializes alpha to 1.0.
        let diff1 = EcnCounts {
            ect0: 0,
            ect1: 9,
            ce: 1,
        };
        prague.on_ecn_delivery(now, diff1);
        prague.on_congestion_event(now, now, false, true, 0, diff1);
        assert_eq!(prague.alpha, Some(1.0));

        // 2. We process ECN ACKs over time.
        // After 30ms (which is > rtt_virt = 25ms), we get an ECN event with 9 ect1 and 1 ce.
        // ce_count = 1, ect_count = 9. frac = 1 / 10 = 0.1.
        // New alpha = 15/16 * 1.0 + 1/16 * 0.1 = 0.94375.
        let diff2 = EcnCounts {
            ect0: 0,
            ect1: 9,
            ce: 1,
        };
        prague.on_ecn_delivery(now + Duration::from_millis(30), diff2);
        prague.on_congestion_event(
            now + Duration::from_millis(30),
            now + Duration::from_millis(30),
            false,
            true,
            0,
            diff2,
        );

        let alpha = prague.alpha.unwrap();
        assert!((alpha - 0.94375).abs() < 1e-5);
    }
}
