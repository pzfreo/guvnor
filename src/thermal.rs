//! Temperature control: a fast PI loop holds the hottest CPU die sensor at a setpoint, and the
//! prototype's fan rules, run once a period on the fan speed, move that setpoint.
//!
//! Why: these fans react to peak die temperature, not to average load. With a 10 s duty cycle a
//! heavy parallel build heats the die from 55 to over 90 °C in each 2 s burst, so the fans spin up
//! even at the 20 % floor. The die answers within about a second, so a short cycle (1 s) with
//! feedback on temperature keeps it steady, and the slow fan loop only has to find the temperature
//! at which the fans stay at base speed.

use crate::config::{Rules, Temperature};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Limit,
    High,
    Off,
    Below,
    Base,
}

/// A change of setpoint, for the log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    pub old: f64,
    pub new: f64,
    pub reason: Reason,
}

#[derive(Debug, Clone)]
pub struct Thermal {
    pub rules: Rules,
    pub t: Temperature,
    duty: f64,
    setpoint: f64,
    err_prev: Option<f64>,
    ticks: u32,
    since_cut: u32,
    recovering: bool,
    last_rpm: u32,
    /// Base-speed rises wait base_every × this; each cut doubles it (up to 8), fans off resets it.
    slow: u32,
    /// The setpoint that last overshot. Below the target, rises are fast only while the setpoint
    /// is well under it: the fans lag the die by minutes, so a fast climb past it overshoots again.
    over: Option<f64>,
}

impl Thermal {
    pub fn new(rules: Rules, t: Temperature, ceiling: f64) -> Thermal {
        let setpoint = t.start.clamp(t.min, t.max);
        // Start low and let the loop raise it: the first readings arrive within a second.
        let duty = (ceiling * 0.5).max(t.floor);
        Thermal {
            rules,
            t,
            duty,
            setpoint,
            err_prev: None,
            ticks: 0,
            since_cut: u32::MAX,
            recovering: false,
            last_rpm: 0,
            slow: 1,
            over: None,
        }
    }

    pub fn duty(&self) -> f64 {
        self.duty
    }

    pub fn setpoint(&self) -> f64 {
        self.setpoint
    }

    pub fn set_config(&mut self, rules: Rules, t: Temperature) {
        self.setpoint = self.setpoint.clamp(t.min, t.max);
        self.duty = self.duty.clamp(t.floor, rules.ceiling);
        self.rules = rules;
        self.t = t;
    }

    /// The fast loop: once a cycle, with the hottest die temperature at the end of the run phase.
    /// Incremental PI, so clamping the duty cannot wind the integral up.
    pub fn inner(&mut self, temp: f64, dt: f64, ceiling: f64) {
        let e = self.setpoint - temp;
        let de = self.err_prev.map_or(0.0, |p| e - p);
        self.err_prev = Some(e);
        self.duty = (self.duty + self.t.kp * de + self.t.ki * e * dt)
            .clamp(self.t.floor, ceiling.max(self.t.floor));
    }

    /// The slow loop: once a period, with the fan speed. The prototype's rules, moving the
    /// setpoint by `step` °C where they moved the duty by points: above LIMIT 4 steps down (and the
    /// duty cut at once), above HIGH 2 down, fans off or well below the target 1 up every
    /// off_every periods, at the target (base speed for a 2500 target) 1 up every base_every. After a cut, wait cut_hold periods (the fans lag the heat) and,
    /// never cut while the fans are falling. The first cut of an overshoot also slows the
    /// base-speed rises, so the probing for a higher setpoint gets rarer once one has failed.
    pub fn outer(&mut self, rpm: u32, target: u32) -> Option<Change> {
        let r = &self.rules;
        let old = self.setpoint;
        self.since_cut = self.since_cut.saturating_add(1);
        let limit = rpm > target + r.limit_above;
        let high = rpm > target + r.high_above;
        if !high {
            self.recovering = false;
        }
        // The fast loop already holds the die, so falling fans mean the setpoint is working: no
        // cut, even above LIMIT, until they stop falling. After a cut, also wait cut_hold periods.
        let falling = rpm < self.last_rpm;
        // More than jitter: the fans are answering heat already.
        let rising = rpm > self.last_rpm + 50;
        let held = falling || (self.recovering && r.cut_hold > 0 && self.since_cut <= r.cut_hold);
        self.last_rpm = rpm;
        let reason;
        if high {
            if held {
                self.ticks = 0;
                return None;
            }
            reason = if limit { Reason::Limit } else { Reason::High };
            self.setpoint -= self.t.step * if limit { 4.0 } else { 2.0 };
            if limit {
                self.duty = (self.duty * r.limit_factor).max(self.t.floor);
            }
            if !self.recovering {
                // Only the first cut of an overshoot says a probe went too far.
                self.slow = (self.slow * 2).min(8);
                self.over = Some(old);
            }
            self.ticks = 0;
            self.since_cut = 0;
            self.recovering = true;
        } else {
            // Fans off: plenty of room, so rise at the fast rate. Well below the target (and the
            // setpoint well under the one that last overshot): rise every period while the fans
            // hold still, and wait while they rise, since they are answering heat already.
            let off = rpm < r.off_below;
            let near = self
                .over
                .is_some_and(|o| self.setpoint + 2.0 * self.t.step >= o);
            let below = !off && rpm + r.high_above < target && !near;
            reason = if off {
                Reason::Off
            } else if below {
                Reason::Below
            } else {
                Reason::Base
            };
            if off {
                self.slow = 1;
                self.over = None;
            }
            if below && rising {
                self.ticks = 0;
                return None;
            }
            self.ticks += 1;
            if self.ticks
                < if below {
                    1
                } else if off {
                    r.off_every
                } else {
                    r.base_every * self.slow
                }
            {
                return None;
            }
            self.ticks = 0;
            self.setpoint += self.t.step;
        }
        self.setpoint = self.setpoint.clamp(self.t.min, self.t.max);
        (self.setpoint != old).then_some(Change {
            old,
            new: self.setpoint,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A die that heats towards `idle + gain` while the work runs and cools towards `idle`
    /// while it is paused (time constant 1.5 s), under a fan that answers the die temperature
    /// smoothed over 30 s: off below 62 °C, base speed 2500 up to 70 °C, then 150 rpm per °C.
    struct Rig {
        temp: f64,
        smooth: f64,
        rpm: f64,
        gain: f64,
    }

    impl Rig {
        fn new(gain: f64) -> Rig {
            Rig {
                temp: 50.0,
                smooth: 50.0,
                rpm: 0.0,
                gain,
            }
        }

        fn step(&mut self, running: bool, dt: f64) {
            let goal = 50.0 + if running { self.gain } else { 0.0 };
            self.temp += (goal - self.temp) * (dt / 1.5).min(1.0);
            self.smooth += (self.temp - self.smooth) * dt / 30.0;
            let want = if self.smooth < 62.0 {
                0.0
            } else {
                2500.0 + (self.smooth - 70.0).max(0.0) * 150.0
            };
            self.rpm += (want - self.rpm).clamp(-150.0 * dt, 150.0 * dt);
        }
    }

    struct Trace {
        temps: Vec<f64>,
        rpms: Vec<u32>,
        duties: Vec<f64>,
        setpoints: Vec<f64>,
    }

    /// Runs `secs` seconds with a 1 s cycle and the slow loop every 10 s.
    fn run(gain: impl Fn(f64) -> f64, secs: usize) -> Trace {
        let t = Temperature::default();
        let mut c = Thermal::new(Rules::default(), t.clone(), 1.0);
        let mut rig = Rig::new(gain(0.0));
        let dt = 0.05;
        let mut out = Trace {
            temps: vec![],
            rpms: vec![],
            duties: vec![],
            setpoints: vec![],
        };
        for s in 0..secs {
            rig.gain = gain(s as f64);
            let steps = (1.0 / dt) as usize;
            let on = (c.duty() * steps as f64).round() as usize;
            let mut peak = rig.temp;
            for i in 0..steps {
                rig.step(i < on, dt);
                if i < on.max(1) {
                    peak = peak.max(rig.temp);
                }
            }
            c.inner(peak, 1.0, 1.0);
            if s % 10 == 9 {
                c.outer(rig.rpm as u32, 2500);
            }
            out.temps.push(peak);
            out.rpms.push(rig.rpm as u32);
            out.duties.push(c.duty());
            out.setpoints.push(c.setpoint());
        }
        out
    }

    #[test]
    fn the_inner_loop_holds_the_setpoint_without_oscillating() {
        // Light, typical and very heavy loads: the die settles near the setpoint either way.
        for gain in [25.0, 60.0, 120.0] {
            let mut c = Thermal::new(Rules::default(), Temperature::default(), 1.0);
            let mut temp: f64 = 50.0;
            let mut temps = vec![];
            for _ in 0..120 {
                // One second at this duty: the die moves most of the way to its new level.
                let goal = 50.0 + gain * c.duty();
                temp += (goal - temp) * 0.5;
                c.inner(temp, 1.0, 1.0);
                temps.push(temp);
            }
            let tail = &temps[60..];
            let lo = tail.iter().cloned().fold(f64::MAX, f64::min);
            let hi = tail.iter().cloned().fold(f64::MIN, f64::max);
            let target = c.setpoint();
            if gain * 1.0 + 50.0 > target {
                assert!(
                    (lo - target).abs() < 1.0 && (hi - target).abs() < 1.0,
                    "gain {gain}: {lo:.1}..{hi:.1} vs {target}"
                );
            }
        }
    }

    #[test]
    fn fans_stay_at_base_speed_under_a_heavy_load() {
        // The live trial's case: a load that heats the die 70 °C above idle at full duty.
        let tr = run(|_| 70.0, 1800);
        let after = 600; // ten minutes
        // The slow loop still probes upward now and then; the probes stay small and brief.
        let peak = *tr.rpms[after..].iter().max().unwrap();
        let over = tr.rpms[after..].iter().filter(|&&r| r > 2600).count();
        assert!(peak <= 2650, "fans reached {peak} rpm");
        assert!(
            over * 100 <= (tr.rpms.len() - after) * 5,
            "{over} s above HIGH"
        );
        assert!(
            tr.rpms[after..].iter().all(|&r| r >= 2500),
            "fans stopped: the duty fell too far"
        );
        // The setpoint settles: it moves at most one step in the last 20 minutes either way.
        let sp = &tr.setpoints[after..];
        let (lo, hi) = (
            sp.iter().cloned().fold(f64::MAX, f64::min),
            sp.iter().cloned().fold(f64::MIN, f64::max),
        );
        assert!(
            hi - lo <= 3.0 * Temperature::default().step,
            "setpoint {lo}..{hi}"
        );
        let d = &tr.duties[after..];
        let (dlo, dhi) = (
            d.iter().cloned().fold(1.0, f64::min),
            d.iter().cloned().fold(0.0, f64::max),
        );
        assert!(dhi - dlo <= 0.08, "duty swings {dlo:.2}..{dhi:.2}");
        // Throughput: the most the fans allow is a die near 70 °C, a duty near 20/70.
        let mean = tr.duties[after..].iter().sum::<f64>() / (tr.duties.len() - after) as f64;
        assert!(mean >= 0.22, "mean duty {mean:.2}");
    }

    #[test]
    fn a_hot_start_comes_down_and_a_lighter_load_gets_more_duty() {
        let tr = run(|s| if s < 900.0 { 70.0 } else { 35.0 }, 1800);
        let heavy = tr.duties[600..900].iter().sum::<f64>() / 300.0;
        let light = tr.duties[1500..].iter().sum::<f64>() / 300.0;
        assert!(light > heavy + 0.2, "light {light:.2} vs heavy {heavy:.2}");
        assert!(*tr.rpms[1200..].iter().max().unwrap() <= 2650);
        let _ = &tr.temps;
    }

    #[test]
    fn fans_coming_down_from_old_heat_do_not_ratchet_the_setpoint() {
        // As on the live trial: started with the fans at 4400 rpm, falling slowly once the die
        // is under control. One cut at most, not one every few periods all the way down.
        let mut c = Thermal::new(Rules::default(), Temperature::default(), 1.0);
        let start = c.setpoint();
        let mut rpm = 4400;
        while rpm > 2500 {
            c.outer(rpm, 2500);
            rpm -= 100;
        }
        assert!(
            c.setpoint() >= start - 4.0,
            "setpoint fell to {}",
            c.setpoint()
        );
    }

    #[test]
    fn fans_well_below_a_high_target_raise_the_setpoint_quickly() {
        // Target 5000 with the fans holding at 2900: one step a period (after the first reading,
        // which looks like a rise from nothing).
        let mut c = Thermal::new(Rules::default(), Temperature::default(), 1.0);
        let start = c.setpoint();
        for _ in 0..11 {
            c.outer(2900, 5000);
        }
        assert_eq!(c.setpoint(), start + 10.0);
        // While the fans rise they are answering heat already: wait for them.
        for rpm in [3000, 3200, 3400, 3600] {
            assert_eq!(c.outer(rpm, 5000), None);
        }
        assert_eq!(c.setpoint(), start + 10.0);
        c.outer(3600, 5000);
        assert_eq!(c.setpoint(), start + 11.0);
        // At a 2500 target, base speed is at the target: the slow rate.
        let mut c = Thermal::new(Rules::default(), Temperature::default(), 1.0);
        for _ in 0..30 {
            c.outer(2550, 2500);
        }
        assert_eq!(c.setpoint(), start + 5.0);
    }

    #[test]
    fn after_an_overshoot_the_climb_back_slows_near_the_setpoint_that_caused_it() {
        // As overnight at a 4000 target: the setpoint climbed while the lagging
        // fans sat below the target, reached 95 °C, and the fans shot to 5000 every 15 minutes.
        let t = Temperature {
            start: 95.0,
            ..Temperature::default()
        };
        let mut c = Thermal::new(Rules::default(), t, 1.0);
        c.outer(4900, 4000);
        assert_eq!(c.setpoint(), 91.0);
        c.outer(5000, 4000);
        c.outer(4700, 4000);
        for _ in 0..4 {
            c.outer(4800, 4000);
        }
        assert_eq!(c.setpoint(), 87.0);
        // Fans fall below the target: fast up to 2 steps short of 95, then the slow rate.
        let mut periods = 0;
        while c.setpoint() < 93.0 {
            c.outer(3600, 4000);
            periods += 1;
        }
        assert_eq!(periods, 6);
        for _ in 0..11 {
            c.outer(3600, 4000);
        }
        assert_eq!(
            c.setpoint(),
            93.0,
            "base_every × slow (12 periods) per step"
        );
        c.outer(3600, 4000);
        assert_eq!(c.setpoint(), 94.0);
        // Fans off: the load has gone, so the fast rate again.
        for _ in 0..3 {
            c.outer(1500, 4000);
        }
        assert_eq!(c.setpoint(), 95.0);
    }

    #[test]
    fn limit_cuts_the_duty_at_once_and_lowers_the_setpoint() {
        let mut c = Thermal::new(Rules::default(), Temperature::default(), 1.0);
        let (d, sp) = (c.duty(), c.setpoint());
        let ch = c.outer(2800, 2500).unwrap();
        assert_eq!(ch.reason, Reason::Limit);
        assert!((c.duty() - d * 0.7).abs() < 1e-9 && c.setpoint() == sp - 4.0);
        // Held for cut_hold periods.
        assert_eq!(c.outer(2900, 2500), None);
    }
}
