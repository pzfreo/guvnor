//! A per-machine model of the fans: how fast they will be running a minute from now. The fans
//! follow the heat being produced, smoothed over a few minutes (measured on a Mac17,7: r = 0.96),
//! so the model learns the speed they run at for each level of slowly smoothed heat, and
//! predicts their movement: from where they are now, by however much that level will change
//! in the next minute as the recent heat feeds into it. With steady heat that is no change.
//! The curve differs by machine and drifts with the room, so it is learned as the governor
//! runs and kept in the state directory; it is acted on only while it predicts clearly better
//! than assuming the fans stay as they are.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// The heat is smoothed two ways: over this many seconds, the heat lately...
const RECENT: f64 = 30.0;
/// ...and over this many, what the fans follow (chosen by replaying a Mac17,7's history).
const SLOW: f64 = 240.0;
/// Predictions are for this many seconds ahead.
pub const AHEAD: f64 = 60.0;
/// The model's format; a saved one of another version is started afresh.
const VERSION: u32 = 2;
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
    #[serde(default)]
    pub version: u32,
    /// What the heat is measured by: an SMC power key such as "PSTR", or "die" (the mean die
    /// temperature) where there is no power reading.
    pub input: String,
    /// Bin of slowly smoothed heat → (fan rpm, samples).
    bins: BTreeMap<i32, (f64, u32)>,
    /// Predictions checked against what the fans then did.
    pub checked: u32,
    /// The mean error of those predictions, recently weighted, in rpm...
    pub err: f64,
    /// ...and of the guess that the fans stay as they are, which the model has to beat.
    #[serde(default)]
    pub still: f64,
    /// The heat lately and slowly smoothed, and the fan speed, at the last reading.
    #[serde(skip)]
    now: Option<(f64, f64, u32)>,
    /// Recent (time, prediction made then, fan speed then), awaiting the fan speed AHEAD later.
    #[serde(skip)]
    pending: VecDeque<(f64, Option<f64>, u32)>,
}

impl Model {
    pub fn new(input: &str) -> Model {
        Model {
            version: VERSION,
            input: input.into(),
            ..Model::default()
        }
    }

    /// A saved model, if it is current and measures heat by `input`.
    pub fn fits(&self, input: &str) -> bool {
        self.version == VERSION && self.input == input
    }

    /// Takes a reading at time `t` (seconds): the heat now and the fan speed now. It trains the
    /// curve, and checks the prediction made AHEAD earlier.
    pub fn observe(&mut self, t: f64, x: f64, rpm: u32, dt: f64) {
        let ema = |old: Option<f64>, tau: f64| match old {
            Some(o) => o + (1.0 - (-dt.max(0.0) / tau).exp()) * (x - o),
            None => x,
        };
        let recent = ema(self.now.map(|n| n.0), RECENT);
        let slow = ema(self.now.map(|n| n.1), SLOW);
        self.now = Some((recent, slow, rpm));
        let mut due = None;
        while self.pending.front().is_some_and(|p| p.0 <= t - AHEAD) {
            due = self.pending.pop_front();
        }
        if let Some((_, Some(p), then)) = due {
            let rpm = rpm as f64;
            let w = 1.0 / (self.checked + 1).min(SETTLED) as f64;
            self.err += w * ((p - rpm).abs() - self.err);
            self.still += w * ((then as f64 - rpm).abs() - self.still);
            self.checked = self.checked.saturating_add(1);
        }
        let bin = self
            .bins
            .entry((slow / BIN).round() as i32)
            .or_insert((rpm as f64, 0));
        bin.1 = bin.1.saturating_add(1);
        let w = if bin.1 < SETTLED {
            1.0 / bin.1 as f64
        } else {
            RATE
        };
        bin.0 += w * (rpm as f64 - bin.0);
        self.pending.push_back((t, self.predict(), rpm));
    }

    /// The fan speed expected AHEAD from now: the fans now, moved by the change in the curve as
    /// the slowly smoothed heat moves towards the recent heat over the next AHEAD.
    pub fn predict(&self) -> Option<f64> {
        let (recent, slow, rpm) = self.now?;
        let ahead = slow + (recent - slow) * (1.0 - (-AHEAD / SLOW).exp());
        Some(rpm as f64 + self.curve(ahead)? - self.curve(slow)?)
    }

    /// Learned enough, and predicting well enough, to act on: close, and clearly better than
    /// assuming the fans stay as they are.
    pub fn ready(&self) -> bool {
        self.checked >= READY_SAMPLES && self.err <= READY_ERR && self.err < 0.8 * self.still
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
            format!(", ±{:.0} rpm (no change ±{:.0})", self.err, self.still)
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

    /// Heat x watts, held, settles the fans at 2500 + 100 per watt above 20.
    fn fans(x: f64) -> u32 {
        (2500.0 + (x - 20.0).max(0.0) * 100.0) as u32
    }

    /// A machine whose fans follow the heat smoothed over 4 minutes.
    struct Rig {
        t: f64,
        felt: f64,
    }

    impl Rig {
        fn new(heat: f64) -> Rig {
            Rig { t: 0.0, felt: heat }
        }

        /// Runs `secs` of 10 s periods with the heat from `heat(t)`.
        fn run(&mut self, m: &mut Model, secs: f64, heat: impl Fn(f64) -> f64) {
            let end = self.t + secs;
            while self.t < end {
                let x = heat(self.t);
                self.felt += (1.0 - (-10.0f64 / 240.0).exp()) * (x - self.felt);
                m.observe(self.t, x, fans(self.felt), 10.0);
                self.t += 10.0;
            }
        }
    }

    #[test]
    fn learns_the_curve_and_predicts_a_minute_ahead() {
        let mut m = Model::new("PSTR");
        let mut rig = Rig::new(40.0);
        // Heat wandering between 20 and 60 W, and stepping now and then.
        rig.run(&mut m, 4.0 * 3600.0, |t| {
            40.0 + 15.0 * (t / 900.0).sin()
                + if (t / 600.0) as i64 % 2 == 0 {
                    5.0
                } else {
                    -5.0
                }
        });
        assert!(m.ready(), "{}", m.summary(true));
        // Settle at 40 W, then step to 60 W: 40 s in, the fans have barely moved, but the
        // prediction for a minute on is close to where they get to.
        rig.run(&mut m, 1800.0, |_| 40.0);
        rig.run(&mut m, 40.0, |_| 60.0);
        let now = fans(rig.felt) as f64;
        let p = m.predict().unwrap();
        let mut later = Model::default();
        rig.run(&mut later, 60.0, |_| 60.0);
        let then = fans(rig.felt) as f64;
        assert!(
            (p - then).abs() < 0.5 * (now - then).abs(),
            "fans {now:.0}, predicted {p:.0}, a minute later {then:.0}"
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
        Rig::new(40.0).run(&mut m, 3600.0, |_| 40.0);
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
        assert!(back.fits("PSTR") && !back.fits("die"));
        // Steady heat: no change predicted.
        assert!((m.predict().unwrap() - m.now.unwrap().2 as f64).abs() < 1.0);
    }

    #[test]
    fn not_ready_unless_it_beats_assuming_no_change() {
        // Fans that hold still whatever the heat does: the model has nothing to add.
        let mut m = Model::new("PSTR");
        let mut t = 0.0;
        for i in 0..800 {
            let heat = if (i / 7) % 2 == 0 { 30.0 } else { 60.0 };
            m.observe(t, heat, 4000 + (i % 3) * 10, 10.0);
            t += 10.0;
        }
        assert!(
            m.checked >= READY_SAMPLES && !m.ready(),
            "{}",
            m.summary(true)
        );
    }

    #[test]
    fn not_ready_until_checked_for_an_hour() {
        let mut m = Model::new("die");
        Rig::new(40.0).run(&mut m, 1800.0, |t| 40.0 + (t / 300.0).sin() * 5.0);
        assert!(!m.ready());
        assert!(m.summary(true).starts_with("learning from die"));
    }
}
