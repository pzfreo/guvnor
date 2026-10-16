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
    /// The process that started this work: see `owners`. Set by `classify`, else 0.
    pub owner: i32,
    /// Why it counts as work: "owner", "name", "exe", "cmdline", "tree" or "pid" (chosen in the
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
        owner: 0,
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
    argv(pid).map(|(exe, args)| (exe, args.join(" ")))
}

fn argv(pid: i32) -> Option<(String, Vec<String>)> {
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
    Some((exe, args))
}

/// The current directory of a process, if we may see it.
pub fn cwd(pid: i32) -> Option<String> {
    let mut vi: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_vnodepathinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut vi as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    // libc declares the MAXPATHLEN buffer as 32 × 32 bytes.
    let path = vi.pvi_cdir.vip_path.as_flattened();
    (n == size).then(|| c_str(path)).filter(|s| !s.is_empty())
}

/// What hosts sessions rather than doing work: terminals' login, ssh and multiplexers.
fn is_host(name: &str) -> bool {
    matches!(
        name,
        "login" | "sshd" | "sshd-session" | "tmux" | "screen" | "mosh-server" | "zellij"
    )
}

fn is_shell(name: &str) -> bool {
    matches!(
        name.trim_start_matches('-'),
        "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu"
    )
}

/// The owner of each process: what you started that led to it. Walking down from launchd, it
/// is the first process that is not a terminal app, login, ssh, a multiplexer or the shell
/// they run. So a Claude Code session in a terminal tab owns everything its tools run, a
/// command typed into a tab owns its children, and an app owns its helpers.
pub fn owners(procs: &[Proc]) -> HashMap<i32, i32> {
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut out = HashMap::new();
    for p in procs {
        let mut path = vec![p];
        let mut seen = HashSet::from([p.pid]);
        while let Some(&q) = by_pid.get(&path.last().unwrap().ppid) {
            if q.pid <= 1 || !seen.insert(q.pid) {
                break;
            }
            path.push(q);
        }
        path.reverse();
        let mut owner = p.pid;
        // A parent we cannot see is root's: a terminal's login, or sshd.
        let top = path[0];
        let mut after_host = top.ppid > 1 && !by_pid.contains_key(&top.ppid);
        for (i, q) in path.iter().enumerate() {
            let next_is_login = path.get(i + 1).is_some_and(|n| n.name == "login");
            if is_host(&q.name) || next_is_login {
                after_host = true;
            } else if !(after_host && is_shell(&q.name)) {
                owner = q.pid;
                break;
            }
        }
        out.insert(p.pid, owner);
    }
    out
}

/// An owner's name for rules and the dashboard: its executable's file name as it was run.
/// Not the registered name: Claude Code registers its version (2.1.301) there.
pub fn owner_name(p: &Proc, args: &impl Fn(i32) -> Option<(String, String)>) -> String {
    args(p.pid)
        .and_then(|(exe, _)| {
            let base = exe.rsplit('/').next()?;
            (!base.is_empty()).then(|| base.to_string())
        })
        .unwrap_or_else(|| p.name.clone())
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
    owners: Option<Regex>,
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
            owners: any_of(&w.owners, true),
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
        let owner_of = owners(&procs);
        let mut owner_names: HashMap<i32, String> = HashMap::new();
        if self.owners.is_some() {
            for &o in owner_of.values() {
                owner_names.entry(o).or_insert_with(|| {
                    by_pid
                        .get(&o)
                        .map(|op| owner_name(op, &args))
                        .unwrap_or_default()
                });
            }
        }
        // Work started by a chosen owner: everything under it, not the owner itself (often
        // what you interact with, like a Claude Code session).
        let chosen_owner = |p: &Proc| {
            let o = owner_of.get(&p.pid).copied().unwrap_or(p.pid);
            o != p.pid
                && by_pid.get(&o).is_some_and(|op| {
                    pins.owners.contains(&(op.pid, op.start))
                        || self
                            .owners
                            .as_ref()
                            .is_some_and(|r| owner_names.get(&o).is_some_and(|n| r.is_match(n)))
                })
        };
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
            let reason = if chosen_owner(p) {
                Some("owner")
            } else if is(&self.names, &p.name) {
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
                    owner: owner_of.get(&p.pid).copied().unwrap_or(p.pid),
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
            owner: 0,
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
            ..Default::default()
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
    fn owners_are_what_you_started_in_a_tab_or_the_app() {
        let procs = vec![
            p(1, 0, 0, "launchd"),
            p(10, 1, 501, "ghostty"),
            p(11, 10, 0, "login"),
            p(12, 11, 501, "zsh"),
            p(13, 12, 501, "claude"), // a session in a tab
            p(14, 13, 501, "zsh"),    // its tool's shell
            p(15, 14, 501, "cargo"),
            p(16, 15, 501, "sheets-0123456789abcdef"),
            p(20, 11, 501, "zsh"), // another tab
            p(21, 20, 501, "cargo"),
            p(22, 21, 501, "rustc"),
            p(30, 1, 501, "tmux"),
            p(31, 30, 501, "bash"),
            p(32, 31, 501, "python3.14"),
            p(40, 1, 501, "Google Chrome"),
            p(41, 40, 501, "Google Chrome Helper"),
            p(50, 1, 501, "codex"),
        ];
        let o = owners(&procs);
        let of = |pid| o[&pid];
        assert_eq!([of(13), of(14), of(15), of(16)], [13; 4]);
        assert_eq!([of(21), of(22)], [21, 21]);
        assert_eq!(of(32), 32);
        assert_eq!([of(40), of(41)], [40, 40]);
        assert_eq!(of(50), 50);
        assert_eq!(of(12), 12); // a tab's idle shell owns itself
    }

    #[test]
    fn a_chosen_owner_controls_its_work_but_not_itself() {
        let procs = vec![
            p(1, 0, 0, "launchd"),
            p(10, 1, 501, "ghostty"),
            p(11, 10, 0, "login"),
            p(12, 11, 501, "zsh"),
            p(13, 12, 501, "claude"),
            p(14, 13, 501, "zsh"),
            p(15, 14, 501, "node"),
            p(16, 15, 501, "rustc"),
            p(20, 11, 501, "zsh"),
            p(21, 20, 501, "claude"), // another session
            p(22, 21, 501, "python3"),
        ];
        let classify = |w: &Work, pins: &crate::state::Pins| -> Vec<(i32, &'static str)> {
            Matcher::new(w)
                .classify_in(procs.clone(), 999, 501, |_| None, pins)
                .iter()
                .filter(|p| p.controlled())
                .map(|p| (p.pid, p.reason))
                .collect()
        };
        let pins = crate::state::Pins {
            owners: vec![(13, 91)],
            off: vec![(16, 112)], // one process released wins over its owner
            ..Default::default()
        };
        assert_eq!(
            classify(&Work::default(), &pins),
            vec![(14, "owner"), (15, "owner")]
        );
        // By name: every claude session.
        let w = Work {
            owners: vec!["claude".into()],
            ..Work::default()
        };
        let all = classify(&w, &Default::default());
        assert_eq!(
            all,
            vec![(14, "owner"), (15, "owner"), (16, "owner"), (22, "owner")]
        );
    }

    #[test]
    fn reads_this_process() {
        let me = info(std::process::id() as i32).unwrap();
        assert!(!me.stopped);
        let (exe, line) = args(me.pid).unwrap();
        assert!(exe.contains("guvnor") && line.contains("guvnor"));
        assert!(cpu_ns(me.pid).unwrap() > 0);
        assert!(cwd(me.pid).is_some());
    }
}
