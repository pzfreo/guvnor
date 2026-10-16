//! A per-machine model of the fans: how fast they will be running a minute from now, given the
//! heat being produced now. The fans follow heat output smoothed over about a minute and act
//! on it about a minute late (measured on a Mac17,7: r = 0.93), so the heat now predicts the fans
//! a minute ahead. The curve from heat to fan speed differs by machine and drifts with the room,
//! so it is learned as the governor runs, and kept in the state directory.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// The heat input is smoothed over this many seconds...
pub const SMOOTH: f64 = 30.0;
/// ...and predicts the fan speed this many seconds later.
pub const AHEAD: f64 = 60.0;
/// Width of a curve bin, in the input's units (watts, or °C for the die fallback).
const BIN: f64 = 1.0;
/// Once a bin has this many samples, each new one moves it this fraction of the way, so the
/// curve keeps following the machine and the room.
const SETTLED: u32 = 50;
const RATE: f64 = 1.0 / SETTLED as f64;
/// Ready when it has learned for an hour of periods and its predictions are this close.
const READY_SAMPLES: u32 = 360;
const READY_ERR: f64 = 250.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Model {
    /// What the heat is measured by: an SMC power key such as "PSTR", or "die" (the mean die
    /// temperature) where there is no power reading.
    pub input: String,
    /// Bin → (fan rpm, samples).
    bins: BTreeMap<i32, (f64, u32)>,
    /// Predictions checked against what the fans then did.
    pub checked: u32,
    /// The mean error of those predictions, recently weighted, in rpm.
    pub err: f64,
    #[serde(skip)]
    smoothed: Option<f64>,
    /// Recent (time, smoothed input, prediction made then), awaiting the fan speed AHEAD later.
    #[serde(skip)]
    pending: VecDeque<(f64, f64, Option<f64>)>,
}

impl Model {
    pub fn new(input: &str) -> Model {
        Model {
            input: input.into(),
            ..Model::default()
        }
    }

    /// Takes a reading at time `t` (seconds): the heat input now and the fan speed now. The fan
    /// speed trains the input seen AHEAD earlier, and checks the prediction made then.
    pub fn observe(&mut self, t: f64, x: f64, rpm: u32, dt: f64) {
        let a = 1.0 - (-dt.max(0.0) / SMOOTH).exp();
        let s = match self.smoothed {
            Some(s) => s + a * (x - s),
            None => x,
        };
        self.smoothed = Some(s);
        let mut due = None;
        while self.pending.front().is_some_and(|p| p.0 <= t - AHEAD) {
            due = self.pending.pop_front();
        }
        if let Some((_, old, predicted)) = due {
            let rpm = rpm as f64;
            if let Some(p) = predicted {
                let w = 1.0 / (self.checked + 1).min(SETTLED) as f64;
                self.err += w * ((p - rpm).abs() - self.err);
                self.checked = self.checked.saturating_add(1);
            }
            let bin = self
                .bins
                .entry((old / BIN).round() as i32)
                .or_insert((rpm, 0));
            bin.1 = bin.1.saturating_add(1);
            let w = if bin.1 < SETTLED {
                1.0 / bin.1 as f64
            } else {
                RATE
            };
            bin.0 += w * (rpm - bin.0);
        }
        self.pending.push_back((t, s, self.curve(s)));
    }

    /// The fan speed expected AHEAD from now.
    pub fn predict(&self) -> Option<f64> {
        self.curve(self.smoothed?)
    }

    /// Learned enough, and predicting well enough, to act on.
    pub fn ready(&self) -> bool {
        self.checked >= READY_SAMPLES && self.err <= READY_ERR
    }

    /// The curve at `x`: more heat never means slower fans, so each bin counts as at least the
    /// ones below it; between bins it is linear, and past the ends it stays at the end values.
    fn curve(&self, x: f64) -> Option<f64> {
        let mut top = f64::MIN;
        let pts: Vec<(f64, f64)> = self
            .bins
            .iter()
            .filter(|(_, (_, n))| *n >= 3)
            .map(|(&b, &(rpm, _))| {
                top = top.max(rpm);
                (b as f64 * BIN, top)
            })
            .collect();
        let (first, last) = (pts.first()?, pts.last()?);
        if x <= first.0 {
            return Some(first.1);
        }
        if x >= last.0 {
            return Some(last.1);
        }
        let i = pts.iter().position(|p| p.0 >= x)?;
        let (lo, hi) = (pts[i - 1], pts[i]);
        Some(lo.1 + (hi.1 - lo.1) * (x - lo.0) / (hi.0 - lo.0))
    }

    /// One line for the dashboard and `guvnor status`.
    pub fn summary(&self, acting: bool) -> String {
        let fit = if self.checked > 0 {
            format!(", ±{:.0} rpm", self.err)
        } else {
            String::new()
        };
        if self.ready() && acting {
            format!("predicting from {}{fit}", self.input)
        } else if self.ready() {
            format!("learned from {}{fit} (recording only)", self.input)
        } else {
            format!(
                "learning from {}: {} of {} checks{fit}",
                self.input,
                self.checked.min(READY_SAMPLES),
                READY_SAMPLES
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Heat x watts settles the fans at 2500 + 100 per watt above 20, a minute later.
    fn fans(x: f64) -> u32 {
        (2500.0 + (x - 20.0).max(0.0) * 100.0) as u32
    }

    /// Runs `secs` of 10 s periods with the heat from `heat(t)`; the fans follow the heat a
    /// minute late.
    fn run(m: &mut Model, from: f64, secs: f64, heat: impl Fn(f64) -> f64) {
        let mut t = from;
        while t < from + secs {
            m.observe(t, heat(t), fans(heat(t - AHEAD)), 10.0);
            t += 10.0;
        }
    }

    #[test]
    fn learns_the_curve_and_predicts_a_minute_ahead() {
        let mut m = Model::new("PSTR");
        // Heat wandering between 20 and 60 W, slowly enough that smoothing barely lags.
        let heat = |t: f64| 40.0 + 20.0 * (t / 900.0).sin();
        run(&mut m, 0.0, 4.0 * 3600.0, heat);
        assert!(m.ready(), "checked {} err {:.0}", m.checked, m.err);
        // A step up to 55 W: the prediction moves at once, the fans only a minute later.
        let t0 = 4.0 * 3600.0;
        for i in 0..4 {
            m.observe(t0 + i as f64 * 10.0, 55.0, fans(40.0), 10.0);
        }
        let p = m.predict().unwrap();
        assert!(
            p > 4500.0,
            "predicted {p:.0} after the step; fans still at {}",
            fans(40.0)
        );
    }

    #[test]
    fn more_heat_never_predicts_slower_fans() {
        let mut m = Model::new("PSTR");
        for i in 0..20 {
            m.observe(i as f64 * 70.0, 30.0, 4000, 1000.0);
            m.observe(i as f64 * 70.0 + 35.0, 31.0, 3000, 1000.0); // noise: lower at more heat
        }
        assert!(m.curve(31.0).unwrap() >= m.curve(30.0).unwrap());
    }

    #[test]
    fn keeps_following_a_change_and_survives_a_restart() {
        let mut m = Model::new("PSTR");
        run(&mut m, 0.0, 3600.0, |_| 40.0);
        let before = m.curve(40.0).unwrap();
        // The room warms: the same heat now needs 800 rpm more.
        let mut t = 3600.0;
        for _ in 0..300 {
            m.observe(t, 40.0, fans(40.0) + 800, 10.0);
            t += 10.0;
        }
        let after = m.curve(40.0).unwrap();
        assert!(after > before + 700.0, "{before:.0} -> {after:.0}");
        let back: Model = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.curve(40.0), m.curve(40.0));
        assert_eq!(back.input, "PSTR");
    }

    #[test]
    fn not_ready_until_checked_for_an_hour() {
        let mut m = Model::new("die");
        run(&mut m, 0.0, 1800.0, |t| 40.0 + (t / 300.0).sin() * 5.0);
        assert!(!m.ready());
        assert!(m.summary(true).starts_with("learning from die"));
    }
}
