//! `guvnor run`: the control loop.

use crate::config::{Config, Control};
use crate::controller::Controller;
use crate::power::Power;
use crate::procs::{self, Matcher};
use crate::smc::Smc;
use crate::state::{self, Sample};
use crate::thermal::Thermal;
use crate::{log, watchdog};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// Samples kept for the dashboard: an hour at the default period.
const HISTORY: usize = 360;

/// Sleeps in short slices; returns false early when asked to exit or when paused.
fn nap(secs: f64) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
    loop {
        if STOP.load(Ordering::SeqCst) || state::paused() {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn pct(d: f64) -> String {
    format!("{:.0}%", d * 100.0)
}

fn continue_all(matcher: &Matcher) -> usize {
    let n = state::release_stopped();
    let work = matcher.find();
    for p in &work {
        procs::signal(p.pid, libc::SIGCONT);
    }
    n.max(work.len())
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The duty comes from the fan rules directly (the prototype), or from the temperature loop
/// whose setpoint the fan rules move.
enum Ctl {
    Fans(Controller),
    Temp(Thermal, Vec<[u8; 4]>),
}

impl Ctl {
    fn new(config: &Config, smc: &Smc, rpm: u32, ceiling: f64) -> Ctl {
        if config.control == Control::Temperature {
            let keys = smc.resolve(&config.temperature.keys);
            if !keys.is_empty() {
                return Ctl::Temp(
                    Thermal::new(config.rules.clone(), config.temperature.clone(), ceiling),
                    keys,
                );
            }
            log(&format!(
                "warning: no temperature sensors match {:?}; using the fan rules",
                config.temperature.keys
            ));
        }
        Ctl::Fans(Controller::new(config.rules.clone(), rpm, ceiling))
    }

    fn duty(&self) -> f64 {
        match self {
            Ctl::Fans(c) => c.duty(),
            Ctl::Temp(t, _) => t.duty(),
        }
    }

    /// Seconds per duty cycle.
    fn cycle(&self, config: &Config) -> f64 {
        match self {
            Ctl::Fans(_) => config.period,
            Ctl::Temp(..) => config.temperature.period,
        }
    }

    fn floor(&self, config: &Config) -> f64 {
        match self {
            Ctl::Fans(_) => config.rules.floor,
            Ctl::Temp(..) => config.temperature.floor,
        }
    }

    /// The fan rules, once a period.
    fn outer(&mut self, rpm: u32, target: u32, ceiling: f64) {
        match self {
            Ctl::Fans(c) => {
                if let Some(ch) = c.update(rpm, target, ceiling) {
                    log(&format!(
                        "fan {rpm} rpm: duty {} -> {} ({:?})",
                        pct(ch.old),
                        pct(ch.new),
                        ch.reason
                    ));
                }
            }
            Ctl::Temp(t, _) => {
                if let Some(ch) = t.outer(rpm, target) {
                    log(&format!(
                        "fan {rpm} rpm: setpoint {:.0} -> {:.0} °C ({:?}), duty {}",
                        ch.old,
                        ch.new,
                        ch.reason,
                        pct(t.duty())
                    ));
                }
            }
        }
    }

    /// The temperature loop, once a cycle; returns the reading.
    fn inner(&mut self, smc: &Smc, dt: f64, ceiling: f64) -> Option<f64> {
        match self {
            Ctl::Fans(_) => None,
            Ctl::Temp(t, keys) => {
                let temp = smc.hottest(keys)?;
                t.inner(temp, dt, ceiling);
                Some(temp)
            }
        }
    }

    fn setpoint(&self) -> Option<f64> {
        match self {
            Ctl::Fans(_) => None,
            Ctl::Temp(t, _) => Some(t.setpoint()),
        }
    }
}

/// What the loop publishes for `guvnor status` and the dashboard.
struct Publisher {
    history: VecDeque<Sample>,
    rpm: u32,
    target: u32,
    ceiling: f64,
    temp: Option<f64>,
}

impl Publisher {
    fn write(&self, ctl: &Ctl, cfg: &Config, phase: &str, secs: f64) {
        state::write_status(&state::Status {
            pid: std::process::id() as i32,
            updated: state::now(),
            rpm: self.rpm,
            duty: ctl.duty(),
            cap: match ctl {
                Ctl::Fans(c) => c.cap(),
                Ctl::Temp(..) => None,
            },
            target: self.target,
            ceiling: self.ceiling,
            period: cfg.period,
            paused: phase == "off",
            phase: phase.into(),
            phase_end: state::now() + secs,
            history: self.history.iter().copied().collect(),
            events: crate::recent_events(),
            control: match ctl {
                Ctl::Fans(_) => "fans".into(),
                Ctl::Temp(..) => "temperature".into(),
            },
            temp: self.temp,
            setpoint: ctl.setpoint(),
            cycle: ctl.cycle(cfg),
        });
    }
}

pub fn run(path: PathBuf) -> Result<(), String> {
    let mut config = Config::load(&path)?;
    let _lock = state::lock()?;
    let smc = Smc::open().ok_or("cannot open the SMC (AppleSMC)")?;
    let mut matcher = Matcher::new(&config.work);
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
    // Anything left paused by an earlier governor that died runs again first.
    let n = continue_all(&matcher);
    let mut dog = watchdog::spawn().map_err(|e| format!("cannot start the watchdog: {e}"))?;

    let mut power = Power::current();
    let (target, ceiling) = config.effective(power);
    let rpm = smc.rpm();
    let mut ctl = Ctl::new(&config, &smc, rpm, ceiling);
    let mut was_paused = state::paused();
    let mut seen: HashSet<i32> = HashSet::new();
    let mut mtime = modified(&path);
    let mut out = Publisher {
        history: VecDeque::new(),
        rpm,
        target,
        ceiling,
        temp: None,
    };
    let how = match &ctl {
        Ctl::Fans(_) => format!(
            "fan rules, duty {} of each {} s",
            pct(ctl.duty()),
            config.period
        ),
        Ctl::Temp(t, keys) => format!(
            "temperature, holding {} sensors at {:.0} °C with a {} s cycle",
            keys.len(),
            t.setpoint(),
            config.temperature.period
        ),
    };
    log(&format!(
        "start: fan {rpm} rpm, target {target} rpm, {how}, all cores; continued {n} work processes{}",
        if was_paused { "; paused" } else { "" }
    ));

    let mut next_outer = Instant::now();
    let mut hot_at_floor = 0u32;
    while !STOP.load(Ordering::SeqCst) {
        if let Ok(Some(_)) = dog.try_wait() {
            log("watchdog exited; restarting it");
            dog = watchdog::spawn().map_err(|e| format!("cannot restart the watchdog: {e}"))?;
        }
        let paused = state::paused();
        if paused != was_paused {
            log(if paused {
                "paused: throttling off"
            } else {
                "resumed: throttling on"
            });
            was_paused = paused;
        }

        // Once a period: the config, the power source, the fans and the fan rules.
        if Instant::now() >= next_outer {
            next_outer = Instant::now() + Duration::from_secs_f64(config.period);
            let now_mtime = modified(&path);
            if now_mtime != mtime {
                mtime = now_mtime;
                match Config::load(&path) {
                    Ok(c) if c == config => {}
                    Ok(c) => {
                        let mode_changed = c.control != config.control
                            || c.temperature.keys != config.temperature.keys;
                        config = c;
                        matcher = Matcher::new(&config.work);
                        (out.target, out.ceiling) = config.effective(power);
                        match &mut ctl {
                            _ if mode_changed => {
                                ctl = Ctl::new(&config, &smc, smc.rpm(), out.ceiling)
                            }
                            Ctl::Fans(c) => c.set_rules(config.rules.clone()),
                            Ctl::Temp(t, _) => {
                                t.set_config(config.rules.clone(), config.temperature.clone())
                            }
                        }
                        log(&format!(
                            "config reloaded: target {} rpm, period {} s, ceiling {}",
                            out.target,
                            config.period,
                            pct(out.ceiling)
                        ));
                    }
                    Err(e) => log(&format!("config error, keeping the previous settings: {e}")),
                }
            }
            let now_power = Power::current();
            if now_power != power {
                power = now_power;
                (out.target, out.ceiling) = config.effective(power);
                let src = if power.on_battery { "battery" } else { "AC" };
                log(&format!(
                    "power: {src}{}, target {} rpm, ceiling {}",
                    power.charge.map(|c| format!(" {c}%")).unwrap_or_default(),
                    out.target,
                    pct(out.ceiling)
                ));
            }
            out.rpm = smc.rpm();
            if !paused {
                ctl.outer(out.rpm, out.target, out.ceiling);
            }
            // The die far above the setpoint with the work already at the floor for a minute:
            // the heat is coming from something guvnor does not throttle. Say so once.
            let starved = match (&ctl, out.temp) {
                (Ctl::Temp(t, _), Some(temp)) => {
                    !paused
                        && t.duty() <= config.temperature.floor + 1e-9
                        && temp > t.setpoint() + 10.0
                }
                _ => false,
            };
            hot_at_floor = if starved { hot_at_floor + 1 } else { 0 };
            if hot_at_floor == 6 {
                log(&format!(
                    "warning: die {:.0} °C with the work at the floor for a minute: the heat comes from \
                     processes guvnor does not match (see top, and [work] in the config)",
                    out.temp.unwrap_or_default()
                ));
            }
            if out.history.len() >= HISTORY {
                out.history.pop_front();
            }
            out.history.push_back(Sample {
                t: state::now(),
                rpm: out.rpm,
                duty: if paused { 1.0 } else { ctl.duty() },
                temp: out.temp,
                setpoint: ctl.setpoint(),
            });
        }

        let cycle = ctl.cycle(&config);
        let duty = ctl.duty();
        // Paused, or at full duty, nothing is stopped: the fans are polled, and in temperature
        // mode the die is read once a cycle to know when to start.
        if paused || duty >= 1.0 - 1e-9 {
            out.write(&ctl, &config, if paused { "off" } else { "full" }, cycle);
            let end = Instant::now() + Duration::from_secs_f64(cycle);
            while Instant::now() < end && !STOP.load(Ordering::SeqCst) && state::paused() == paused
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            if !paused && let Some(t) = ctl.inner(&smc, cycle, out.ceiling) {
                out.temp = Some(t);
            }
            continue;
        }

        // Run for duty × cycle, then read the die at its hottest.
        out.write(&ctl, &config, "run", duty * cycle);
        if !nap(duty * cycle) {
            continue;
        }
        if let Some(t) = ctl.inner(&smc, cycle, out.ceiling) {
            out.temp = Some(t);
        }

        // Pause for the rest. Builds spawn short-lived processes, so look for work now. Work
        // found is moved out of the background band once. A process already stopped by someone
        // else (Ctrl-Z) is left alone, so it is not resumed later.
        let pause = cycle * (1.0 - duty);
        let work = matcher.find();
        seen.retain(|pid| work.iter().any(|p| p.pid == *pid));
        for p in &work {
            if seen.insert(p.pid) && !procs::not_background(p.pid) && procs::alive(p.pid) {
                log(&format!(
                    "warning: cannot move {} ({}) out of the background band",
                    p.pid, p.name
                ));
            }
        }
        let pids: Vec<i32> = work
            .into_iter()
            .filter(|p| !p.stopped)
            .map(|p| p.pid)
            .collect();
        out.write(&ctl, &config, "pause", pause);
        if pids.is_empty() {
            nap(pause);
            continue;
        }
        let stopped = state::Stopped {
            governor: std::process::id() as i32,
            deadline: state::now() + pause.min(cycle * (1.0 - ctl.floor(&config))),
            pids: pids.clone(),
        };
        if let Err(e) = state::write_stopped(&stopped) {
            log(&format!(
                "error: cannot record the paused processes, not pausing: {e}"
            ));
            nap(pause);
            continue;
        }
        for &pid in &pids {
            procs::signal(pid, libc::SIGSTOP);
        }
        nap(pause);
        for &pid in &pids {
            procs::signal(pid, libc::SIGCONT);
        }
        state::clear_stopped();
    }

    let n = continue_all(&matcher);
    log(&format!("exit: continued {n} work processes"));
    let _ = dog.kill();
    let _ = dog.wait();
    Ok(())
}
