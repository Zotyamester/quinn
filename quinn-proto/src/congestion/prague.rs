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
    window: u64,
    ect1_enabled: bool,
}

impl Prague {
    /// Following the Linux Prague reference implementation's choice of constants according to
    /// 2.4.4. Reduced RTT-Dependence (draft-briscoe-iccrg-prague-congestion-control-04)
    const RTT_VIRT_MIN: Duration = Duration::from_millis(25);

    /// Construct a state using the given `config` and current time `now`
    pub fn new(config: Arc<PragueConfig>, now: Instant, current_mtu: u16) -> Self {
        let cubic_config = Arc::new(CubicConfig::default());
        let cubic = Cubic::new(cubic_config, now, current_mtu);
        let window = cubic.window();

        Self {
            config,
            cubic,
            window,
            ect1_enabled: false,
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
        let rtt_virt = rtt.get().max(Prague::RTT_VIRT_MIN);
        // TODO: While implementing Prague, be ware of this rtt_virt calculation! Furthermore, use
        // it to limit the update of the fraction and EWMA to once per virtual RTT.
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        is_ecn: bool,
        lost_bytes: u64,
        counts: &EcnCounts,
    ) {
        if !self.ect1_enabled {
            self.cubic.on_congestion_event(now, sent, is_persistent_congestion, is_ecn, lost_bytes, counts);
            return;
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.cubic.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        self.window
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
pub struct PragueConfig {
    loss_reduction_factor: f32,
}

impl PragueConfig {
    /// Reduction in congestion window when a new loss event is detected.
    pub fn loss_reduction_factor(&mut self, value: f32) -> &mut Self {
        self.loss_reduction_factor = value;
        self
    }
}

impl Default for PragueConfig {
    fn default() -> Self {
        Self {
            loss_reduction_factor: 0.5,
        }
    }
}

impl ControllerFactory for PragueConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Prague::new(self, now, current_mtu))
    }
}
