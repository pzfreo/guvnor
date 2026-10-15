//! fangov: keeps the fans at or below a target speed by duty-cycling only the work processes
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
mod watchdog;

use std::io::Write;

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/".into())
}

/// A timestamped line on stdout (the LaunchAgent sends it to ~/Library/Logs/fangov.log).
fn log(msg: &str) {
    let mut buf = [0u8; 32];
    let t = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%F %T".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{t} {msg}");
    let _ = out.flush();
}

const USAGE: &str = "usage: fangov <command>

  run         run the governor in the foreground, logging to stdout
  status      show the fans, the duty and the matched work processes
  pause       stop throttling and continue all work (persists until resume)
  resume      start throttling again
  install     add a per-user LaunchAgent (KeepAlive) that runs `fangov run`
  uninstall   remove it, continuing all work
  config      print a commented config file with the defaults
  fans        print the fan speeds, as fan.c does

config: ~/.config/fangov/config.toml (or $FANGOV_CONFIG)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Printing commands exit quietly into a closed pipe (`fangov status | head`). The governor
    // keeps ignoring SIGPIPE, so a lost log can never kill it without continuing the work.
    if matches!(
        args.first().map(String::as_str),
        Some("status" | "config" | "fans")
    ) {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    let result = match args.first().map(String::as_str) {
        Some("run") => load().and_then(governor::run),
        Some("status") => load().map(|c| status(&c)),
        Some("pause") => load().and_then(|c| pause(&c)),
        Some("resume") => state::set_paused(false)
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
        Some("watchdog") => match args.get(1).and_then(|p| p.parse().ok()) {
            Some(pid) => watchdog::run(pid),
            None => Err("usage: fangov watchdog <governor pid>".into()),
        },
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("fangov: {e}");
        std::process::exit(1);
    }
}

fn load() -> Result<config::Config, String> {
    config::Config::load(&config::default_path())
}

/// The running governor's status, if one is running.
fn running() -> Option<state::Status> {
    state::read_status()
        .filter(|s| procs::alive(s.pid) && procs::info(s.pid).is_some_and(|p| p.name == "fangov"))
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
                "paused (throttling off)".to_string()
            } else {
                format!("duty {:.0}%{cap}", s.duty * 100.0)
            };
            println!(
                "governor: running (pid {}), {state}, target {} rpm, period {} s",
                s.pid, s.target, s.period
            );
        }
        None => println!(
            "governor: not running{}",
            if state::paused() {
                " (pause flag set)"
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
            "  {:>7} {:<8} {}",
            p.pid,
            if p.stopped { "paused" } else { "running" },
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
    println!("throttling off; all work continued (fangov resume to turn it back on)");
    Ok(())
}
