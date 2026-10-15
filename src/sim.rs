//! A lagging fan for testing the controller, after what the prototype saw on a Mac17,7: the fans
//! are off or at least at a 2500–2599 rpm base speed, and follow heat tens of seconds late. Heat
//! passes through two first-order lags (the die, then the SMC's smoothing) to the demanded speed.

use crate::config::Rules;
use crate::controller::Controller;

pub struct FanSim {
    /// Heat from the work while it runs, relative to a full build.
    pub load: f64,
    h1: f64,
    h2: f64,
    rpm: f64,
    on: bool,
    seed: u64,
}

impl FanSim {
    pub fn new() -> FanSim {
        FanSim {
            load: 1.0,
            h1: 0.0,
            h2: 0.0,
            rpm: 0.0,
            on: false,
            seed: 1,
        }
    }

    /// Warm, with the fans at base speed.
    pub fn spinning(rpm: f64) -> FanSim {
        let h = (rpm - 1500.0) / 1800.0;
        FanSim {
            h1: h,
            h2: h,
            rpm,
            on: true,
            ..FanSim::new()
        }
    }

    fn noise(&mut self) -> f64 {
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.seed >> 33) % 100) as f64
    }

    /// One second, with the work running or paused. Full load demands 3300 rpm; 70 % about 2760,
    /// the overshoot the prototype saw at 70 % duty.
    pub fn step(&mut self, running: bool) {
        let input = if running { self.load } else { 0.0 };
        self.h1 += (input - self.h1) / 40.0;
        self.h2 += (self.h1 - self.h2) / 20.0;
        let demand = 1500.0 + 1800.0 * self.h2;
        if self.on && demand < 2250.0 {
            self.on = false;
        }
        if !self.on && demand >= 2500.0 {
            self.on = true;
        }
        let want = if self.on {
            demand.max(2500.0 + self.noise())
        } else {
            0.0
        };
        self.rpm += (want - self.rpm).clamp(-150.0, 150.0);
    }

    pub fn reading(&self) -> u32 {
        self.rpm as u32
    }
}

pub struct Run {
    pub rpms: Vec<u32>,
    pub duties: Vec<f64>,
}

pub fn simulate(rules: Rules, periods: usize, load: impl Fn(usize) -> f64) -> Run {
    run(rules, periods, load, FanSim::new())
}

pub fn simulate_from(rules: Rules, periods: usize, load: impl Fn(usize) -> f64, rpm: u32) -> Run {
    run(rules, periods, load, FanSim::spinning(rpm as f64))
}

/// Runs the controller against the fan with a 10 s period, as the governor would.
fn run(rules: Rules, periods: usize, load: impl Fn(usize) -> f64, mut sim: FanSim) -> Run {
    let mut ctl = Controller::new(rules, sim.reading(), 1.0);
    let mut out = Run {
        rpms: vec![],
        duties: vec![],
    };
    for p in 0..periods {
        sim.load = load(p);
        if p > 0 {
            ctl.update(sim.reading(), 2500, 1.0);
        }
        let on = (ctl.duty() * 10.0).round() as usize;
        for s in 0..10 {
            sim.step(s < on);
        }
        out.duties.push(ctl.duty());
        out.rpms.push(sim.reading());
    }
    out
}
