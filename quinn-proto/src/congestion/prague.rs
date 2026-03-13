use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use super::{Controller, ControllerFactory};
use crate::Instant;
use crate::congestion::{Cubic, CubicConfig};
use crate::connection::RttEstimator;
use crate::frame::EcnCounts;

/// A simple, standard congestion controller
#[derive(Debug, Clone)]
pub struct Prague {
    config: Arc<PragueConfig>,
    cubic: Cubic,
    /// Maximum number of bytes in flight that may be sent.
    ect1_enabled: bool,
    ect_count: f64,
    ce_count: f64,
    last_alpha_update: Instant,
    alpha: Option<f64>,
    rtt_virt: Duration,
}

impl Prague {
    /// Following the Linux Prague reference implementation's choice of constants according to
    /// 2.4.4. Reduced RTT-Dependence (draft-briscoe-iccrg-prague-congestion-control-04)
    const RTT_VIRT_MIN: Duration = Duration::from_millis(25);

    const EWMA_GAIN: f64 = 1.0 / 16.0;

    /// Construct a state using the given `config` and current time `now`
    pub fn new(config: Arc<PragueConfig>, now: Instant, current_mtu: u16) -> Self {
        let cubic_config = Arc::new(CubicConfig::default());
        let cubic = Cubic::new(cubic_config, now, current_mtu);

        Self {
            config,
            cubic,
            ect1_enabled: false,
            ect_count: 0.0,
            ce_count: 0.0,
            last_alpha_update: Instant::now(), // this
            alpha: None,
            rtt_virt: Prague::RTT_VIRT_MIN,
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
        // TODO: While implementing Prague, be ware of this rtt_virt calculation! Furthermore, use
        // it to limit the update of the fraction and EWMA to once per virtual RTT.
        self.cubic.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        is_ecn: bool,
        lost_bytes: u64,
        diff: EcnCounts,
    ) {
        if !self.ect1_enabled {
            self.cubic.on_congestion_event(
                now,
                sent,
                is_persistent_congestion,
                is_ecn,
                lost_bytes,
                diff,
            );
            return;
        }
        if let Some(alpha) = self.alpha {
            self.ect_count += diff.ect1 as f64;
            self.ce_count += diff.ce as f64;
            if now - self.last_alpha_update > self.rtt_virt {
                let frac = self.ce_count / (self.ce_count + self.ect_count);
                self.alpha = Some((1.0 - Prague::EWMA_GAIN) * alpha + Prague::EWMA_GAIN * frac);
                self.last_alpha_update = now;
                self.ect_count = 0.0;
                self.ce_count = 0.0;
            }
        } else if diff.ce > 0 {
            self.last_alpha_update = now;
            self.alpha = Some(1.0);
            self.ect_count = diff.ect1 as f64;
            self.ce_count = diff.ce as f64;
        }

        self.cubic
            .set_window((self.cubic.window() as f64 * self.alpha.unwrap()) as u64);

        // last congestion stuff???
        // here i'm starting to get lost in Google's QUICHE/Prague impl
        if diff.ce == 0 || lost_bytes > 0 {
            self.cubic.on_congestion_event(
                now,
                sent,
                is_persistent_congestion,
                is_ecn,
                lost_bytes,
                diff,
            );
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.cubic.on_mtu_update(new_mtu);
    }

    fn set_window(&mut self, size: u64) {
        self.cubic.set_window(size);
    }

    fn window(&self) -> u64 {
        self.cubic.window()
    }

    fn metrics(&self) -> super::ControllerMetrics {
        self.cubic.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.cubic.initial_window()
    }

    fn enable_ect0(&mut self) -> bool {
        self.cubic.enable_ect0()
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
