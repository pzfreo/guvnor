//! guvnor: keeps the fans at or below a target speed by duty-cycling only the work processes
//! (builds, tests, renders) with SIGSTOP/SIGCONT. See README.md.

mod config;
mod controller;
mod governor;
mod launchd;
mod power;
mod procs;
#[cfg(test)]
mod sim;
mod smc;
mod state;
mod thermal;
mod tui;
mod watchdog;

use std::io::Write;

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/".into())
}

/// A timestamped line on stdout (the LaunchAgent sends it to ~/Library/Logs/guvnor.log).
fn log(msg: &str) {
    let mut buf = [0u8; 32];
    let t = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%F %T".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    let line = format!("{t} {msg}");
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    if events.len() >= 50 {
        events.pop_front();
    }
    events.push_back(line);
}

/// The latest log lines, for the status file.
static EVENTS: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

fn recent_events() -> Vec<String> {
    EVENTS
        .lock()
        .map(|e| e.iter().cloned().collect())
        .unwrap_or_default()
}

const USAGE: &str = "usage: guvnor [command]

  (none)      the live dashboard (or status, when not in a terminal)
  run         run the governor in the foreground, logging to stdout
  status      show the fans, the duty and the matched work processes
  off         throttling off: all work runs freely (persists until `on`; also `pause`)
  on          throttling on again (also `resume`)
  install     add a per-user LaunchAgent (KeepAlive) that runs `guvnor run`
  uninstall   remove it, continuing all work
  config      print a commented config file with the defaults
  fans        print the fan speeds, as fan.c does
  keys        print every SMC key with a numeric value (to choose temperature sensors)

config: ~/.config/guvnor/config.toml (or $GUVNOR_CONFIG)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Printing commands exit quietly into a closed pipe (`guvnor status | head`). The governor
    // keeps ignoring SIGPIPE, so a lost log can never kill it without continuing the work.
    let tty = unsafe { libc::isatty(1) == 1 && libc::isatty(0) == 1 };
    if matches!(
        args.first().map(String::as_str),
        Some("status" | "config" | "fans")
    ) || (args.is_empty() && !tty)
    {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    let result = match args.first().map(String::as_str) {
        Some("run") => governor::run(config::default_path()),
        None if tty => tui::run(),
        None | Some("status") => load().map(|c| status(&c)),
        Some("off" | "pause") => load().and_then(|c| pause(&c)).map(|_| {
            println!("throttling off; all work runs freely (guvnor on to turn it back on)")
        }),
        Some("on" | "resume") => state::set_paused(false)
            .map_err(|e| e.to_string())
            .map(|_| println!("throttling on")),
        Some("install") => launchd::install(),
        Some("uninstall") => load().and_then(|c| launchd::uninstall(&c)),
        Some("config") => {
            print!("{}", config::SAMPLE);
            Ok(())
        }
        Some("fans") => smc::Smc::open()
            .map(|s| println!("{}", smc::describe(&s.fans())))
            .ok_or("cannot open the SMC".into()),
        Some("keys") => smc::Smc::open()
            .ok_or("cannot open the SMC".into())
            .map(|s| {
                for k in s.keys() {
                    if let Some(v) = s.number(&k) {
                        println!("{} {v:.2}", String::from_utf8_lossy(&k));
                    }
                }
            }),
        // Diagnostic, not in the usage: `guvnor sample <secs> <key or prefix*>...` prints the fastest fan and the
        // maximum of each key group once a second.
        Some("sample") => smc::Smc::open()
            .ok_or("cannot open the SMC".into())
            .map(|s| {
                // <secs>[@<interval ms>]
                let spec = args.get(1).cloned().unwrap_or_default();
                let (secs, every) = spec.split_once('@').unwrap_or((&spec, "1000"));
                let secs: u64 = secs.parse().unwrap_or(60);
                let every: u64 = every.parse().unwrap_or(1000);
                let all = s.keys();
                let groups: Vec<(String, Vec<[u8; 4]>)> = args[2..]
                    .iter()
                    .map(|g| {
                        let pre = g.trim_end_matches('*').as_bytes();
                        let keys = all
                            .iter()
                            .filter(|k| {
                                if g.ends_with('*') {
                                    k.starts_with(pre)
                                } else {
                                    &k[..] == pre
                                }
                            })
                            .copied()
                            .collect();
                        (g.clone(), keys)
                    })
                    .collect();
                println!(
                    "t rpm phase {}",
                    groups
                        .iter()
                        .map(|g| g.0.clone())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                let start = std::time::Instant::now();
                while start.elapsed().as_secs() < secs {
                    let phase = state::read_status().map(|st| st.phase).unwrap_or_default();
                    let vals: Vec<String> = groups
                        .iter()
                        .map(|(_, ks)| {
                            let m = ks
                                .iter()
                                .filter_map(|k| s.number(k))
                                .filter(|v| *v > 0.0 && *v < 150.0)
                                .fold(f64::NAN, f64::max);
                            format!("{m:.1}")
                        })
                        .collect();
                    println!(
                        "{:.2} {} {phase} {}",
                        start.elapsed().as_secs_f64(),
                        s.rpm(),
                        vals.join(" ")
                    );
                    std::thread::sleep(std::time::Duration::from_millis(every));
                }
            }),
        Some("watchdog") => match args.get(1).and_then(|p| p.parse().ok()) {
            Some(pid) => watchdog::run(pid),
            None => Err("usage: guvnor watchdog <governor pid>".into()),
        },
        Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            eprintln!("guvnor: unknown command {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("guvnor: {e}");
        std::process::exit(1);
    }
}

fn load() -> Result<config::Config, String> {
    config::Config::load(&config::default_path())
}

/// The running governor's status, if one is running.
fn running() -> Option<state::Status> {
    state::read_status()
        .filter(|s| procs::alive(s.pid) && procs::info(s.pid).is_some_and(|p| p.name == "guvnor"))
}

/// Resumes processes a dead governor left paused. Returns how many were listed.
fn sweep_orphans() -> usize {
    match state::read_stopped() {
        Some(s) if !procs::alive(s.governor) => state::release_stopped(),
        _ => 0,
    }
}

fn status(c: &config::Config) {
    let n = sweep_orphans();
    if n > 0 {
        println!("continued {n} processes left paused by a governor that died");
    }
    let power = power::Power::current();
    match running() {
        Some(s) => {
            let cap = s
                .cap
                .map(|c| format!(", cap {:.0}%", c * 100.0))
                .unwrap_or_default();
            let state = if s.paused {
                "throttling off".to_string()
            } else {
                let avg = s.avg.map(|a| format!(" (avg {a:.0})")).unwrap_or_default();
                let die = match (s.temp, s.setpoint) {
                    (Some(t), Some(sp)) => format!(", die {t:.0} °C{avg} holding {sp:.0} °C"),
                    _ => String::new(),
                };
                format!("duty {:.0}%{cap}{die}", s.duty * 100.0)
            };
            println!(
                "governor: running (pid {}), {state}, target {} rpm, period {} s",
                s.pid, s.target, s.period
            );
        }
        None => println!(
            "governor: not running{}",
            if state::paused() {
                " (throttling off)"
            } else {
                ""
            }
        ),
    }
    match smc::Smc::open() {
        Some(smc) => println!("fans: {} ({} rpm)", smc::describe(&smc.fans()), smc.rpm()),
        None => println!("fans: cannot open the SMC"),
    }
    let src = if power.on_battery { "battery" } else { "AC" };
    println!(
        "power: {src}{}",
        power
            .charge
            .map(|c| format!(", {c}% charged"))
            .unwrap_or_default()
    );
    let work = procs::Matcher::new(&c.work).find();
    println!("work: {} processes", work.len());
    for p in &work {
        println!(
            "  {:>7} {:<8} {:<8} {}",
            p.pid,
            if p.stopped { "paused" } else { "running" },
            p.reason,
            p.name
        );
    }
}

fn pause(c: &config::Config) -> Result<(), String> {
    state::set_paused(true).map_err(|e| e.to_string())?;
    // The governor checks the flag every 100 ms; continue everything twice to cover a pause
    // that was starting just as the flag was set.
    let matcher = procs::Matcher::new(&c.work);
    for _ in 0..2 {
        state::release_stopped();
        for p in matcher.find() {
            procs::signal(p.pid, libc::SIGCONT);
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Ok(())
}
