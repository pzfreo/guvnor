//! The duty-cycle controller from the prototype (reference/fangov.py): one update per period.
//!
//! The prototype's rules are unchanged: above LIMIT ×0.7, above HIGH ×0.9, fans off +10 points
//! every 3 periods, base speed +5 every 6, between floor and ceiling. Against a lagging fan they
//! settle into a limit cycle (climb at base speed until the fans pass HIGH, then several cuts in a
//! row while the fans catch up). Two additions stop it:
//! - after a cut, further cuts wait `cut_hold` periods, since the readings still show the old duty,
//!   and then only happen if the fans are not already falling;
//! - a cut remembers the duty that overshot, and rises stop one base step below it. That cap is
//!   probed one step higher after `relearn_every` periods at base speed (backing off if the probe
//!   overshoots), and faster while the fans are off.

use crate::config::Rules;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Limit,
    High,
    Off,
    Base,
    Relearn,
    Ceiling,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    pub old: f64,
    pub new: f64,
    pub reason: Reason,
}

#[derive(Debug, Clone)]
pub struct Controller {
    pub rules: Rules,
    duty: f64,
    /// Rises stop at this duty; None until an overshoot is seen.
    cap: Option<f64>,
    ticks: u32,
    since_cut: u32,
    at_cap: u32,
    /// Periods since the cap was last probed higher, while that probe is unresolved.
    probing: Option<u32>,
    backoff: u32,
    recovering: bool,
    last_rpm: u32,
}

const MAX_BACKOFF: u32 = 8;

impl Controller {
    /// A restart while the fans spin starts at half duty rather than at full.
    pub fn new(rules: Rules, start_rpm: u32, ceiling: f64) -> Controller {
        let duty = if start_rpm < rules.off_below {
            ceiling
        } else {
            ceiling.min(0.5).max(rules.floor)
        };
        Controller {
            rules,
            duty,
            cap: None,
            ticks: 0,
            since_cut: u32::MAX,
            at_cap: 0,
            probing: None,
            backoff: 1,
            recovering: false,
            last_rpm: start_rpm,
        }
    }

    /// New rules from a reloaded config; the duty and what was learned carry over.
    pub fn set_rules(&mut self, rules: Rules) {
        self.duty = self.duty.clamp(rules.floor, rules.ceiling);
        if rules.relearn_every == 0 {
            self.cap = None;
        }
        self.rules = rules;
    }

    pub fn duty(&self) -> f64 {
        self.duty
    }

    pub fn cap(&self) -> Option<f64> {
        self.cap
    }

    /// Applies one period's fan reading. Returns the change when the duty moved by a point or more.
    pub fn update(&mut self, rpm: u32, target: u32, ceiling: f64) -> Option<Change> {
        let r = self.rules.clone();
        let old = self.duty;
        let off = rpm < r.off_below;
        self.since_cut = self.since_cut.saturating_add(1);
        // Until the fans come back below HIGH after a cut, they are still catching up with the
        // duty before it: cut again only after the hold, and below LIMIT only if they are not
        // already falling.
        let limit = rpm > target + r.limit_above;
        let held = self.recovering
            && r.cut_hold > 0
            && (self.since_cut <= r.cut_hold || (!limit && rpm < self.last_rpm));
        self.last_rpm = rpm;
        self.probing = self.probing.map(|p| p + 1);
        let mut reason = if off { Reason::Off } else { Reason::Base };

        if rpm <= target + r.high_above {
            self.recovering = false;
        }
        if rpm > target + r.high_above {
            if held {
                self.ticks = 0;
            } else {
                self.cut(if limit { r.limit_factor } else { r.high_factor });
                reason = if limit { Reason::Limit } else { Reason::High };
            }
        } else if let Some(cap) = self.cap.filter(|c| self.duty >= c - 1e-9) {
            // At the learned cap: hold, and now and then try one step higher.
            self.at_cap += 1;
            let wait = if off {
                r.base_every
            } else {
                r.relearn_every * self.backoff
            };
            if self.at_cap >= wait {
                if self.probing.is_some() {
                    self.backoff = 1; // the last probe held
                }
                let next = cap + if off { r.off_step } else { r.base_step };
                self.cap = (next < ceiling).then_some(next);
                self.duty = next.min(ceiling);
                self.at_cap = 0;
                self.probing = (!off).then_some(0);
                reason = Reason::Relearn;
            }
            if off {
                self.backoff = 1;
            }
        } else if self.duty < ceiling {
            self.ticks += 1;
            let (step, every) = if off {
                (r.off_step, r.off_every)
            } else {
                (r.base_step, r.base_every)
            };
            if self.ticks >= every {
                self.duty = (self.duty + step).min(ceiling).min(self.cap.unwrap_or(1.0));
                self.ticks = 0;
            }
        }
        if self.duty > ceiling {
            self.duty = ceiling;
            reason = Reason::Ceiling;
        }
        ((self.duty * 100.0).round() != (old * 100.0).round()).then_some(Change {
            old,
            new: self.duty,
            reason,
        })
    }

    fn cut(&mut self, factor: f64) {
        let r = &self.rules;
        if self.probing.take().is_some() {
            self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        }
        if r.relearn_every == 0 {
            self.cap = None; // the prototype's behaviour: no learned cap
        } else {
            self.cap = Some(
                self.cap
                    .unwrap_or(1.0)
                    .min(self.duty - r.base_step)
                    .max(r.floor),
            );
        }
        self.duty = (self.duty * factor).max(r.floor);
        self.ticks = 0;
        self.at_cap = 0;
        self.since_cut = 0;
        self.recovering = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{Run, simulate};

    fn legacy() -> Rules {
        Rules {
            cut_hold: 0,
            relearn_every: 0,
            ..Rules::default()
        }
    }

    #[test]
    #[ignore]
    fn dump() {
        let load = |p: usize| if (240..480).contains(&p) { 0.7 } else { 1.0 };
        let run = simulate(Rules::default(), 720, load);
        for (i, (r, d)) in run.rpms.iter().zip(&run.duties).enumerate() {
            print!("{i}:{r}/{:.0}  ", d * 100.0);
            if i % 10 == 9 {
                println!();
            }
        }
    }

    #[test]
    fn prototype_rules_are_kept() {
        let mut c = Controller::new(legacy(), 0, 1.0);
        assert_eq!(c.duty(), 1.0);
        assert_eq!(
            c.update(2701, 2500, 1.0).map(|c| c.reason),
            Some(Reason::Limit)
        );
        assert!((c.duty() - 0.7).abs() < 1e-9);
        c.update(2601, 2500, 1.0);
        assert!((c.duty() - 0.63).abs() < 1e-9);
        // Fans off: +10 points on every third period.
        for _ in 0..2 {
            assert_eq!(c.update(0, 2500, 1.0), None);
        }
        assert!((c.update(0, 2500, 1.0).unwrap().new - 0.73).abs() < 1e-9);
        // The floor holds.
        for _ in 0..20 {
            c.update(3000, 2500, 1.0);
        }
        assert!((c.duty() - 0.2).abs() < 1e-9);
        // A restart with the fans spinning starts at half.
        assert_eq!(Controller::new(Rules::default(), 2550, 1.0).duty(), 0.5);
    }

    #[test]
    fn base_speed_rises_five_points_every_six_periods() {
        let mut c = Controller::new(Rules::default(), 2550, 1.0);
        for _ in 0..5 {
            assert_eq!(c.update(2550, 2500, 1.0), None);
        }
        assert!((c.update(2550, 2500, 1.0).unwrap().new - 0.55).abs() < 1e-9);
    }

    #[test]
    fn ceiling_caps_the_duty() {
        let mut c = Controller::new(Rules::default(), 0, 1.0);
        assert_eq!(c.update(0, 2500, 0.5).unwrap().reason, Reason::Ceiling);
        assert_eq!(c.duty(), 0.5);
    }

    /// Count of HIGH or LIMIT readings, and the peak, after `settle` periods.
    fn after(run: &Run, settle: usize, limit: u32) -> (usize, u32) {
        let tail = &run.rpms[settle..];
        (
            tail.iter().filter(|&&r| r > limit).count(),
            *tail.iter().max().unwrap(),
        )
    }

    #[test]
    fn prototype_rules_oscillate_against_a_lagging_fan() {
        // The baseline the additions fix: a sustained cycle above HIGH.
        let run = simulate(legacy(), 360, |_| 1.0);
        let (over, _) = after(&run, 60, 2600);
        assert!(
            over >= 20,
            "expected the prototype's limit cycle, got {over} periods above HIGH"
        );
    }

    #[test]
    fn converges_without_oscillating_at_2500() {
        // Two hours of a steady full build.
        let run = simulate(Rules::default(), 720, |_| 1.0);
        let settle = 120; // 20 minutes
        let (over_high, peak) = after(&run, settle, 2600);
        // The only excursions left are the cap's occasional probes, which stay just above HIGH.
        assert!(peak <= 2650, "peak {peak} rpm after settling");
        assert!(
            over_high * 100 <= (720 - settle) * 5,
            "{over_high} periods above HIGH after settling"
        );
        let proto = simulate(legacy(), 720, |_| 1.0);
        let (proto_over, proto_peak) = after(&proto, settle, 2600);
        assert!(
            over_high * 3 < proto_over && peak < proto_peak,
            "no better than the prototype"
        );
        // The duty settles: after settling it stays within a narrow band.
        let tail = &run.duties[settle..];
        let lo = tail.iter().cloned().fold(1.0, f64::min);
        let hi = tail.iter().cloned().fold(0.0, f64::max);
        assert!(hi - lo <= 0.12, "duty swings {lo:.2}..{hi:.2}");
        // The fans never stop and restart, which would mean the duty fell too far.
        assert!(run.rpms[settle..].iter().all(|&r| r >= 2500));
        // And the throughput is near the most the fans allow (about 61 % in this model).
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        assert!(mean >= 0.52, "mean duty {mean:.2}");
    }

    #[test]
    fn follows_load_changes() {
        // Full load, then a lighter phase, then full again.
        let load = |p: usize| if (240..480).contains(&p) { 0.7 } else { 1.0 };
        let run = simulate(Rules::default(), 720, load);
        let light = &run.duties[400..480];
        let full = &run.duties[180..240];
        let avg = |s: &[f64]| s.iter().sum::<f64>() / s.len() as f64;
        assert!(
            avg(light) > avg(full) + 0.1,
            "light {:.2} vs full {:.2}",
            avg(light),
            avg(full)
        );
        // Back at full load it settles again without staying above HIGH.
        let (over_high, peak) = after(&run, 560, 2600);
        assert!(
            peak <= 2650 && over_high <= 10,
            "peak {peak}, {over_high} periods above HIGH"
        );
    }

    #[test]
    fn starting_with_fans_spinning() {
        let run = crate::sim::simulate_from(Rules::default(), 360, |_| 1.0, 2550);
        assert_eq!(run.duties[0], 0.5);
        let (_, peak) = after(&run, 90, 2600);
        assert!(peak <= 2700);
    }
}
