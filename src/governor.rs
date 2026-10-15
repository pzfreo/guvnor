//! `guvnor run`: the control loop.

use crate::config::Config;
use crate::controller::Controller;
use crate::power::Power;
use crate::procs::{self, Matcher};
use crate::smc::Smc;
use crate::state::{self, Sample};
use crate::{log, watchdog};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// Samples kept for `guvnor tui`: an hour at the default period.
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
        std::thread::sleep(left.min(Duration::from_millis(100)));
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

/// What the loop publishes for `guvnor status` and `guvnor tui`.
struct Publisher {
    history: VecDeque<Sample>,
}

impl Publisher {
    fn write(
        &self,
        ctl: &Controller,
        cfg: &Config,
        (rpm, target, ceiling): (u32, u32, f64),
        phase: &str,
        secs: f64,
    ) {
        state::write_status(&state::Status {
            pid: std::process::id() as i32,
            updated: state::now(),
            rpm,
            duty: ctl.duty(),
            cap: ctl.cap(),
            target,
            ceiling,
            period: cfg.period,
            paused: phase == "off",
            phase: phase.into(),
            phase_end: state::now() + secs,
            history: self.history.iter().copied().collect(),
            events: crate::recent_events(),
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
    let (mut target, mut ceiling) = config.effective(power);
    let rpm = smc.rpm();
    let mut ctl = Controller::new(config.rules.clone(), rpm, ceiling);
    let mut was_paused = state::paused();
    let mut seen: HashSet<i32> = HashSet::new();
    let mut mtime = modified(&path);
    let mut out = Publisher {
        history: VecDeque::new(),
    };
    log(&format!(
        "start: fan {rpm} rpm, target {target} rpm, duty {} of each {} s, all cores; continued {n} work processes{}",
        pct(ctl.duty()),
        config.period,
        if was_paused { "; paused" } else { "" }
    ));

    while !STOP.load(Ordering::SeqCst) {
        if let Ok(Some(_)) = dog.try_wait() {
            log("watchdog exited; restarting it");
            dog = watchdog::spawn().map_err(|e| format!("cannot restart the watchdog: {e}"))?;
        }
        // A changed config applies from this period on; a broken one is reported and ignored.
        let now_mtime = modified(&path);
        if now_mtime != mtime {
            mtime = now_mtime;
            match Config::load(&path) {
                Ok(c) if c == config => {}
                Ok(c) => {
                    config = c;
                    matcher = Matcher::new(&config.work);
                    ctl.set_rules(config.rules.clone());
                    (target, ceiling) = config.effective(power);
                    log(&format!(
                        "config reloaded: target {target} rpm, period {} s, ceiling {}",
                        config.period,
                        pct(ceiling)
                    ));
                }
                Err(e) => log(&format!("config error, keeping the previous settings: {e}")),
            }
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
        let rpm = smc.rpm();
        let now_power = Power::current();
        if now_power != power {
            power = now_power;
            (target, ceiling) = config.effective(power);
            let src = if power.on_battery { "battery" } else { "AC" };
            log(&format!(
                "power: {src}{}, target {target} rpm, ceiling {}",
                power.charge.map(|c| format!(" {c}%")).unwrap_or_default(),
                pct(ceiling)
            ));
        }
        if !paused && let Some(c) = ctl.update(rpm, target, ceiling) {
            log(&format!(
                "fan {rpm} rpm: duty {} -> {} ({:?})",
                pct(c.old),
                pct(c.new),
                c.reason
            ));
        }
        let duty = ctl.duty();
        let period = config.period;
        if out.history.len() >= HISTORY {
            out.history.pop_front();
        }
        out.history.push_back(Sample {
            t: state::now(),
            rpm,
            duty: if paused { 1.0 } else { duty },
        });

        // At full duty, or paused, nothing is stopped: only the fans are polled.
        if paused || duty >= 1.0 - 1e-9 {
            out.write(
                &ctl,
                &config,
                (rpm, target, ceiling),
                if paused { "off" } else { "full" },
                period,
            );
            let end = Instant::now() + Duration::from_secs_f64(period);
            while Instant::now() < end && !STOP.load(Ordering::SeqCst) && state::paused() == paused
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            continue;
        }

        // Run for duty × period. Work found now is moved out of the background band.
        out.write(&ctl, &config, (rpm, target, ceiling), "run", duty * period);
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
        if !nap(duty * period) {
            continue;
        }

        // Pause for the rest. Builds spawn short-lived processes, so look again. A process
        // already stopped by someone else (Ctrl-Z) is left alone, so it is not resumed later.
        let pause = period * (1.0 - duty);
        out.write(&ctl, &config, (rpm, target, ceiling), "pause", pause);
        let pids: Vec<i32> = matcher
            .find()
            .into_iter()
            .filter(|p| !p.stopped)
            .map(|p| p.pid)
            .collect();
        if pids.is_empty() {
            nap(pause);
            continue;
        }
        let stopped = state::Stopped {
            governor: std::process::id() as i32,
            deadline: state::now() + pause.min(period * (1.0 - config.rules.floor)),
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
