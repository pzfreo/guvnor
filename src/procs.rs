//! Finding work processes (like pgrep, through libproc and sysctl) and signalling them.

use crate::config::Work;
use regex::Regex;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub name: String,
    pub stopped: bool,
    zombie: bool,
    /// Start time in microseconds: with the pid, it names one process even after the pid is reused.
    pub start: u64,
    /// Why it counts as work: "name", "exe", "cmdline", "tree" or "pid" (chosen in the
    /// dashboard). Not work: "excluded" (exclude_names), "released" (chosen in the dashboard),
    /// or "" (no rule matches).
    pub reason: &'static str,
}

fn c_str(bytes: &[libc::c_char]) -> String {
    let b: Vec<u8> = bytes
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&b).into_owned()
}

/// One process's BSD info, or None if it has gone or is not visible.
pub fn info(pid: i32) -> Option<Proc> {
    let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut bi as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // The registered name if there is one (it can be longer), else the 16-byte command name.
    let name = match c_str(&bi.pbi_name) {
        n if n.is_empty() => c_str(&bi.pbi_comm),
        n => n,
    };
    Some(Proc {
        pid,
        ppid: bi.pbi_ppid as i32,
        uid: bi.pbi_uid,
        name,
        stopped: bi.pbi_status == libc::SSTOP,
        zombie: bi.pbi_status == 5, // SZOMB
        start: bi.pbi_start_tvsec * 1_000_000 + bi.pbi_start_tvusec,
        reason: "",
    })
}

pub fn all() -> Vec<Proc> {
    let mut pids = vec![0i32; 8192];
    loop {
        let bytes = (pids.len() * size_of::<i32>()) as i32;
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return vec![];
        }
        if (n as usize) < pids.len() {
            pids.truncate(n as usize);
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    pids.into_iter()
        .filter(|&p| p > 0)
        .filter_map(info)
        .collect()
}

/// The executable path as it was run, and the command line (arguments joined by spaces, as
/// `pgrep -f` sees it).
pub fn args(pid: i32) -> Option<(String, String)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = buf.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let buf = &buf[..size];
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    // After argc: the executable path, NUL padding, then argv.
    let mut rest = &buf[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    let exe = String::from_utf8_lossy(&rest[..path_end]).into_owned();
    rest = &rest[path_end..];
    let start = rest.iter().position(|&b| b != 0)?;
    let args: Vec<String> = rest[start..]
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    Some((exe, args.join(" ")))
}

fn any_of(patterns: &[String], whole: bool) -> Option<Regex> {
    if patterns.is_empty() {
        return None;
    }
    let alts: Vec<String> = patterns.iter().map(|p| format!("(?:{p})")).collect();
    let body = alts.join("|");
    Regex::new(&if whole { format!("^(?:{body})$") } else { body }).ok()
}

pub struct Matcher {
    names: Option<Regex>,
    exes: Option<Regex>,
    cmdlines: Option<Regex>,
    tree_names: Option<Regex>,
    tree_cmdlines: Option<Regex>,
    exclude: Option<Regex>,
}

impl Proc {
    /// Whether guvnor throttles it: matched by a rule and not excluded.
    pub fn controlled(&self) -> bool {
        !matches!(self.reason, "" | "excluded" | "released")
    }
}

impl Matcher {
    pub fn new(w: &Work) -> Matcher {
        Matcher {
            names: any_of(&w.names, true),
            exes: any_of(&w.exes, false),
            cmdlines: any_of(&w.cmdlines, false),
            tree_names: any_of(&w.tree_names, true),
            tree_cmdlines: any_of(&w.tree_cmdlines, false),
            exclude: any_of(&w.exclude_names, true),
        }
    }

    /// The current user's work processes. Never this process, its ancestors (the shell and
    /// terminal), its descendants (the watchdog), any other guvnor, or other users' processes.
    pub fn find(&self) -> Vec<Proc> {
        self.classify()
            .into_iter()
            .filter(Proc::controlled)
            .collect()
    }

    /// Every process `find` could consider, with `reason` set to the rule that makes it work,
    /// "excluded" when a rule matches but exclude_names wins, or "" when no rule matches.
    pub fn classify(&self) -> Vec<Proc> {
        let pins = crate::state::read_pins();
        self.classify_in(
            all(),
            std::process::id() as i32,
            unsafe { libc::getuid() },
            args,
            &pins,
        )
    }

    #[cfg(test)]
    pub fn find_in(
        &self,
        procs: Vec<Proc>,
        me: i32,
        uid: u32,
        args: impl Fn(i32) -> Option<(String, String)>,
    ) -> Vec<Proc> {
        let pins = crate::state::Pins::default();
        self.classify_in(procs, me, uid, args, &pins)
            .into_iter()
            .filter(Proc::controlled)
            .collect()
    }

    pub fn classify_in(
        &self,
        procs: Vec<Proc>,
        me: i32,
        uid: u32,
        args: impl Fn(i32) -> Option<(String, String)>,
        pins: &crate::state::Pins,
    ) -> Vec<Proc> {
        let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for p in &procs {
            children.entry(p.ppid).or_default().push(p.pid);
        }
        let descendants = |roots: &mut Vec<i32>| {
            let mut seen = HashSet::new();
            while let Some(pid) = roots.pop() {
                if seen.insert(pid) {
                    roots.extend(
                        children
                            .get(&pid)
                            .into_iter()
                            .flatten()
                            .filter(|&&c| c != pid),
                    );
                }
            }
            seen
        };

        let mut protected = descendants(&mut vec![me]);
        let mut pid = me;
        while let Some(p) = by_pid.get(&pid) {
            protected.insert(pid);
            if p.ppid == pid || p.ppid <= 1 {
                break;
            }
            pid = p.ppid;
        }

        let candidates: Vec<&Proc> = procs
            .iter()
            .filter(|p| {
                p.uid == uid
                    && !p.zombie
                    && p.pid > 1
                    && !protected.contains(&p.pid)
                    && p.name != "guvnor"
            })
            .collect();
        let want_args =
            self.exes.is_some() || self.cmdlines.is_some() || self.tree_cmdlines.is_some();
        let mut work: HashMap<i32, &'static str> = HashMap::new();
        let mut roots = vec![];
        for p in &candidates {
            let (exe, line) = if want_args {
                args(p.pid).unwrap_or_default()
            } else {
                Default::default()
            };
            let is = |r: &Option<Regex>, s: &str| r.as_ref().is_some_and(|r| r.is_match(s));
            let reason = if is(&self.names, &p.name) {
                Some("name")
            } else if is(&self.exes, &exe) {
                Some("exe")
            } else if is(&self.cmdlines, &line) {
                Some("cmdline")
            } else {
                None
            };
            if let Some(r) = reason {
                work.insert(p.pid, r);
            }
            if is(&self.tree_names, &p.name) || is(&self.tree_cmdlines, &line) {
                roots.push(p.pid);
            }
        }
        for pid in descendants(&mut roots) {
            work.entry(pid).or_insert("tree");
        }
        let excluded = |p: &Proc| self.exclude.as_ref().is_some_and(|r| r.is_match(&p.name));
        let mut out: Vec<Proc> = candidates
            .into_iter()
            .map(|p| {
                // Choices for one process win over the config's rules.
                let reason = match work.get(&p.pid) {
                    _ if pins.off.contains(&(p.pid, p.start)) => "released",
                    _ if pins.on.contains(&(p.pid, p.start)) => "pid",
                    Some(_) if excluded(p) => "excluded",
                    Some(&r) => r,
                    None => "",
                };
                Proc {
                    reason,
                    ..p.clone()
                }
            })
            .collect();
        out.sort_by_key(|p| p.pid);
        out
    }
}

/// Total CPU time used, in nanoseconds.
#[allow(deprecated)] // libc's mach_timebase_info is fine; mach2 would be a dependency for one call
pub fn cpu_ns(pid: i32) -> Option<u64> {
    let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_taskinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut ti as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // Mach time units; on Apple silicon 1 unit is 125/3 ns.
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    unsafe { libc::mach_timebase_info(&mut tb) };
    let t = ti.pti_total_user + ti.pti_total_system;
    Some(if tb.denom == 0 {
        t
    } else {
        (t as u128 * tb.numer as u128 / tb.denom as u128) as u64
    })
}

/// Sends a signal; a process that has gone is not an error.
pub fn signal(pid: i32, sig: i32) {
    if pid > 1 {
        unsafe { libc::kill(pid, sig) };
    }
}

/// Whether the process exists and has not exited (a zombie awaiting its parent has).
pub fn alive(pid: i32) -> bool {
    pid > 0 && info(pid).is_some_and(|p| !p.zombie)
}

/// `taskpolicy -B -p <pid>`: moves a process out of the background band, so it is not confined
/// to the efficiency cores. It keeps its normal priority.
pub fn not_background(pid: i32) -> bool {
    unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, uid: u32, name: &str) -> Proc {
        Proc {
            pid,
            ppid,
            uid,
            name: name.into(),
            stopped: false,
            zombie: false,
            start: pid as u64 * 7,
            reason: "",
        }
    }

    #[test]
    fn matches_names_cmdlines_and_trees_but_never_the_governor_side() {
        let procs = vec![
            p(1, 0, 0, "launchd"),
            p(10, 1, 501, "Terminal"),
            p(11, 10, 501, "zsh"),
            p(12, 11, 501, "guvnor"), // me
            p(13, 12, 501, "guvnor"), // my watchdog
            p(20, 11, 501, "cargo"),  // work by name
            p(21, 20, 501, "rustc"),
            p(22, 20, 501, "build-script"), // not matched by name
            p(23, 11, 501, "test-bin"),     // matched by executable path
            p(25, 11, 501, "zsh"),          // only mentions target/ in its arguments
            p(24, 1, 0, "rustc"),           // another user's
            p(30, 11, 501, "make"),         // a tree root
            p(31, 30, 501, "sh"),
            p(32, 31, 501, "cc1"),
            p(40, 11, 501, "ldx"), // names are whole-name matches
        ];
        let w = Work {
            names: [
                "rustc",
                "cargo",
                "ld",
                "clang",
                "cc",
                "cargo-clippy",
                "clippy-driver",
            ]
            .map(String::from)
            .to_vec(),
            exes: vec!["/(release|debug)/".into()],
            tree_names: vec!["make".into()],
            ..Work::default()
        };
        let m = Matcher::new(&w);
        let args = |pid| {
            let (exe, line) = match pid {
                23 => (
                    "/x/target/debug/deps/test-bin",
                    "/x/target/debug/deps/test-bin --exact",
                ),
                25 => ("/bin/zsh", "/bin/zsh -c ./target/release/app"),
                _ => ("", ""),
            };
            Some((exe.to_string(), line.to_string()))
        };
        let found: Vec<(i32, &str)> = m
            .find_in(procs, 12, 501, args)
            .iter()
            .map(|p| (p.pid, p.reason))
            .collect();
        let want = [
            (20, "name"),
            (21, "name"),
            (23, "exe"),
            (30, "tree"),
            (31, "tree"),
            (32, "tree"),
        ];
        assert_eq!(found, want);
    }

    #[test]
    fn exclude_names_wins_over_every_rule() {
        let procs = vec![
            p(11, 1, 501, "zsh"),
            p(20, 11, 501, "cargo"),
            p(21, 20, 501, "rustc"),
        ];
        let w = Work {
            names: vec!["cargo".into(), "rustc".into()],
            exclude_names: vec!["rustc".into()],
            ..Work::default()
        };
        let all: Vec<(i32, &str)> = Matcher::new(&w)
            .classify_in(procs, 999, 501, |_| None, &crate::state::Pins::default())
            .iter()
            .map(|p| (p.pid, p.reason))
            .collect();
        assert_eq!(all, vec![(11, ""), (20, "name"), (21, "excluded")]);
    }

    #[test]
    fn choices_for_one_process_win_and_follow_the_start_time() {
        let procs = vec![
            p(11, 1, 501, "zsh"),
            p(20, 11, 501, "python3.14"),
            p(21, 11, 501, "rustc"),
        ];
        let pins = crate::state::Pins {
            on: vec![(20, 140), (11, 1)], // zsh's entry has the wrong start time: a reused pid
            off: vec![(21, 147)],
        };
        let w = Work {
            names: vec!["rustc".into()],
            ..Work::default()
        };
        let all: Vec<(i32, &str)> = Matcher::new(&w)
            .classify_in(procs, 999, 501, |_| None, &pins)
            .iter()
            .map(|p| (p.pid, p.reason))
            .collect();
        assert_eq!(all, vec![(11, ""), (20, "pid"), (21, "released")]);
    }

    #[test]
    fn a_cargo_process_launching_the_governor_is_protected() {
        // `cargo run -- run` makes cargo an ancestor of the governor.
        let procs = vec![
            p(11, 1, 501, "zsh"),
            p(20, 11, 501, "cargo"),
            p(21, 20, 501, "guvnor"),
        ];
        let w = Work {
            names: vec!["cargo".into()],
            ..Work::default()
        };
        let found = Matcher::new(&w).find_in(procs, 21, 501, |_| None);
        assert!(found.is_empty());
    }

    #[test]
    fn reads_this_process() {
        let me = info(std::process::id() as i32).unwrap();
        assert!(!me.stopped);
        let (exe, line) = args(me.pid).unwrap();
        assert!(exe.contains("guvnor") && line.contains("guvnor"));
        assert!(cpu_ns(me.pid).unwrap() > 0);
    }
}
