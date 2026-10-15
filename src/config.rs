//! `~/.config/guvnor/config.toml`. Every setting is optional; the defaults are the prototype's.

use crate::power::Power;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Keep the fastest fan at or below this. Base speed (2500–2599 on a Mac17,7) counts as within it.
    pub target_rpm: u32,
    /// Seconds per duty cycle: work runs duty × period, then is paused for the rest.
    pub period: f64,
    /// "temperature": a fast loop holds a CPU die temperature that the fan rules move (default).
    /// "fans": the prototype, the fan rules setting the duty directly.
    pub control: Control,
    pub rules: Rules,
    pub temperature: Temperature,
    pub work: Work,
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    Temperature,
    Fans,
}

/// The temperature loop (`control = "temperature"`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Temperature {
    /// SMC sensors, a trailing `*` matching a prefix; the hottest counts. `Tp*`: CPU dies.
    pub keys: Vec<String>,
    /// The die temperature (°C) the loop starts by holding; the fan rules then move it.
    pub start: f64,
    pub min: f64,
    pub max: f64,
    /// °C the fan rules move the setpoint per step (where they moved the duty by points).
    pub step: f64,
    /// Seconds per duty cycle. Short, so a heavy build cannot heat the die in one burst.
    pub period: f64,
    /// PI gains: duty per °C of error, and duty per °C·s.
    pub kp: f64,
    pub ki: f64,
    /// The duty never goes below this. With a 1 s cycle, 0.05 still runs the work 50 ms a second.
    pub floor: f64,
}

impl Default for Temperature {
    fn default() -> Self {
        Temperature {
            keys: vec!["Tp*".into()],
            start: 75.0,
            min: 50.0,
            max: 95.0,
            step: 1.0,
            period: 1.0,
            kp: 0.005,
            ki: 0.005,
            floor: 0.05,
        }
    }
}

/// The duty-cycle rules.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rules {
    /// Fans above target + this: duty × limit_factor (prototype LIMIT 2700 at a 2500 target).
    pub limit_above: u32,
    /// Fans above target + this: duty × high_factor (prototype HIGH 2600).
    pub high_above: u32,
    /// Below this the fans count as off (prototype LOW 2000).
    pub off_below: u32,
    pub limit_factor: f64,
    pub high_factor: f64,
    /// Fans off: duty rises off_step every off_every periods.
    pub off_step: f64,
    pub off_every: u32,
    /// Fans at base speed: duty rises base_step every base_every periods.
    pub base_step: f64,
    pub base_every: u32,
    pub floor: f64,
    pub ceiling: f64,
    /// After a cut, further cuts wait this many periods: the fans lag the heat, so readings just
    /// after a cut still show the duty before it.
    pub cut_hold: u32,
    /// A cut remembers the duty that overshot and rises stop one base_step below it. After this
    /// many periods at that cap at base speed, the cap rises one base_step to find a lighter load
    /// again. A probe that overshoots doubles the wait, up to 8 times. 0 turns the cap off.
    pub relearn_every: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Work {
    /// Regexes matched against the whole process name (like `pgrep -x`).
    pub names: Vec<String>,
    /// Regexes searched for in the executable's path, as it was run.
    pub exes: Vec<String>,
    /// Regexes searched for in the full command line (like `pgrep -f`).
    pub cmdlines: Vec<String>,
    /// A process whose whole name matches is work, and so are all its descendants.
    pub tree_names: Vec<String>,
    /// A process whose command line matches is work, and so are all its descendants.
    pub tree_cmdlines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Battery {
    /// On battery the duty never exceeds this.
    pub max_duty: Option<f64>,
    /// On battery, this target replaces target_rpm.
    pub target_rpm: Option<u32>,
    /// Apply only when the charge is at or below this percentage.
    pub below_charge: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            target_rpm: 2500,
            period: 10.0,
            control: Control::Temperature,
            rules: Rules::default(),
            temperature: Temperature::default(),
            work: Work::default(),
            battery: None,
        }
    }
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            limit_above: 200,
            high_above: 100,
            off_below: 2000,
            limit_factor: 0.7,
            high_factor: 0.9,
            off_step: 0.10,
            off_every: 3,
            base_step: 0.05,
            base_every: 6,
            floor: 0.2,
            ceiling: 1.0,
            cut_hold: 4,
            relearn_every: 30,
        }
    }
}

impl Default for Work {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect();
        Work {
            names: s(&[
                "rustc",
                "cargo",
                "ld",
                "clang",
                "cc",
                "cargo-clippy",
                "clippy-driver",
            ]),
            exes: s(&["target/(release|debug)/"]),
            cmdlines: vec![],
            tree_names: vec![],
            tree_cmdlines: vec![],
        }
    }
}

impl Default for Battery {
    fn default() -> Self {
        Battery {
            max_duty: None,
            target_rpm: None,
            below_charge: 100,
        }
    }
}

pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GUVNOR_CONFIG") {
        return p.into();
    }
    crate::home().join(".config/guvnor/config.toml")
}

impl Config {
    /// Loads the file, or the defaults when it does not exist.
    pub fn load(path: &std::path::Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<(), String> {
        let r = &self.rules;
        let check = |ok: bool, msg: &str| if ok { Ok(()) } else { Err(msg.to_string()) };
        check(self.period >= 1.0, "period must be at least 1 s")?;
        let t = &self.temperature;
        check(
            t.period >= 0.2 && t.period <= self.period,
            "temperature.period must be between 0.2 s and period",
        )?;
        check(
            t.min < t.max && t.step > 0.0 && t.kp >= 0.0 && t.ki > 0.0,
            "temperature: need min < max, step > 0, kp >= 0, ki > 0",
        )?;
        check(
            t.floor > 0.0 && t.floor <= self.rules.ceiling,
            "temperature.floor must be in (0, ceiling]",
        )?;
        check(
            !t.keys.is_empty(),
            "temperature.keys must name at least one sensor",
        )?;
        check(
            r.floor > 0.0 && r.floor <= r.ceiling && r.ceiling <= 1.0,
            "need 0 < floor <= ceiling <= 1",
        )?;
        check(
            r.limit_factor > 0.0 && r.limit_factor < 1.0,
            "limit_factor must be between 0 and 1",
        )?;
        check(
            r.high_factor > 0.0 && r.high_factor < 1.0,
            "high_factor must be between 0 and 1",
        )?;
        check(
            r.off_step > 0.0 && r.base_step > 0.0,
            "steps must be positive",
        )?;
        check(
            r.off_every >= 1 && r.base_every >= 1,
            "*_every must be at least 1",
        )?;
        check(
            r.high_above <= r.limit_above,
            "high_above must not exceed limit_above",
        )?;
        if let Some(d) = self.battery.as_ref().and_then(|b| b.max_duty) {
            check(d > 0.0 && d <= 1.0, "battery.max_duty must be in (0, 1]")?;
        }
        let w = &self.work;
        for p in w
            .names
            .iter()
            .chain(&w.cmdlines)
            .chain(&w.tree_names)
            .chain(&w.tree_cmdlines)
        {
            regex::Regex::new(p).map_err(|e| format!("bad regex {p:?}: {e}"))?;
        }
        Ok(())
    }

    /// The target rpm and duty ceiling that apply on this power source.
    pub fn effective(&self, power: Power) -> (u32, f64) {
        match &self.battery {
            Some(b) if power.on_battery && power.charge.unwrap_or(0) <= b.below_charge => (
                b.target_rpm.unwrap_or(self.target_rpm),
                self.rules
                    .ceiling
                    .min(b.max_duty.unwrap_or(1.0))
                    .max(self.rules.floor),
            ),
            _ => (self.target_rpm, self.rules.ceiling),
        }
    }
}

pub const SAMPLE: &str = r#"# guvnor configuration. Every setting is optional; these are the defaults.

target_rpm = 2500     # keep the fastest fan at or below this (base speed counts as within it)
period = 10           # seconds per duty cycle: work runs duty × period, then is paused

[rules]
limit_above = 200     # fans above target + this: duty × limit_factor
high_above = 100      # fans above target + this: duty × high_factor
off_below = 2000      # fans below this count as off
limit_factor = 0.7
high_factor = 0.9
off_step = 0.10       # fans off: duty rises off_step every off_every periods
off_every = 3
base_step = 0.05      # fans at base speed: duty rises base_step every base_every periods
base_every = 6
floor = 0.2           # duty never goes below this, so no pause exceeds period × (1 − floor)
ceiling = 1.0
cut_hold = 4          # after a cut, wait this many periods before cutting again (fans lag heat)
relearn_every = 30    # rises stop just below the duty that last overshot; after this many
                      # periods there at base speed, try one base_step higher (0: no cap;
                      # with cut_hold = 0 that is exactly the prototype's behaviour)

[work]
# Regexes on the whole process name (like pgrep -x).
names = ["rustc", "cargo", "ld", "clang", "cc", "cargo-clippy", "clippy-driver"]
# Regexes searched for in the executable's path: built programs and test binaries.
exes = ["target/(release|debug)/"]
# Regexes searched for in the full command line (like pgrep -f). This also catches shells and
# scripts that merely mention a path, so prefer exes.
cmdlines = []
# A process matching these, and all its descendants, is work.
tree_names = []
tree_cmdlines = []

# On battery: cap the duty, lower the target, or both. Uncomment to enable.
# [battery]
# max_duty = 0.5
# target_rpm = 2500
# below_charge = 100  # only when the charge is at or below this percentage
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_the_defaults() {
        assert_eq!(Config::parse(SAMPLE).unwrap(), Config::default());
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        assert!(Config::parse("target_rmp = 2400").is_err());
        assert!(Config::parse("[rules]\nfloor = 0").is_err());
        assert!(Config::parse("[work]\nnames = [\"(\"]").is_err());
    }

    #[test]
    fn battery_profile_applies_only_on_battery_below_the_charge() {
        let c =
            Config::parse("[battery]\nmax_duty = 0.5\ntarget_rpm = 0\nbelow_charge = 50").unwrap();
        let ac = Power {
            on_battery: false,
            charge: Some(20),
        };
        assert_eq!(c.effective(ac), (2500, 1.0));
        assert_eq!(
            c.effective(Power {
                on_battery: true,
                charge: Some(80)
            }),
            (2500, 1.0)
        );
        assert_eq!(
            c.effective(Power {
                on_battery: true,
                charge: Some(40)
            }),
            (0, 0.5)
        );
    }
}
