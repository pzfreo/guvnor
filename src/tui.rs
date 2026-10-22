//! `guvnor` with no command: a live dashboard. It reads the fans itself and the governor's status file, so it
//! also works, read-only, when no governor is running.

use crate::config::{self, Config};
use crate::power::Power;
use crate::procs::{self, Proc};
use crate::smc::Smc;
use crate::state::{self, Sample, Status};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Cell, Chart, Dataset, Gauge, GraphType, Paragraph, Row, Table,
};
use ratatui::{DefaultTerminal, Frame};
use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

const ACCENT: Color = Color::Rgb(94, 196, 255);
const DIM: Color = Color::Rgb(110, 118, 129);
const OK: Color = Color::Rgb(87, 211, 140);
const WARN: Color = Color::Rgb(255, 196, 61);
const HOT: Color = Color::Rgb(255, 99, 99);
const DUTY: Color = Color::Rgb(199, 146, 255);
const AVG: Color = Color::Rgb(255, 166, 87);
const LOAD: Color = Color::Rgb(120, 220, 200);

const TARGET: Color = Color::Rgb(70, 140, 90);
const HIGH_LINE: Color = Color::Rgb(150, 115, 30);
const LIMIT_LINE: Color = Color::Rgb(150, 55, 55);

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

struct Work {
    proc: Proc,
    cpu: f64,
}

/// What started some work: see `procs::owners`.
struct Owner {
    pid: i32,
    start: u64,
    name: String,
    /// Its current directory, with the home directory as ~.
    place: String,
    /// CPU of the owner and everything under it.
    total: f64,
    /// "this" (chosen in the dashboard), "all" (a config rule on its name) or "".
    choice: &'static str,
    /// How many of its processes are controlled, by any rule.
    controlled: usize,
    /// How many processes it runs, busy or not.
    count: usize,
    /// Whether its processes are listed below it (→ opens, ← closes).
    open: bool,
}

/// What keeps the table still enough to choose from: lines linger after they go quiet, and
/// the order holds while you are moving around it.
#[derive(Default)]
struct View {
    /// When each line (pid, owner line) was last busy enough to list.
    busy_at: HashMap<(i32, bool), Instant>,
    /// Each line's position last time.
    order: HashMap<(i32, bool), usize>,
    /// Keep the last order, putting new lines last.
    frozen: bool,
    /// Owners opened with →, to list their processes.
    open: HashSet<i32>,
}

/// How long a line stays after it goes quiet.
const LINGER: Duration = Duration::from_secs(60);
/// How long after a key press the order holds.
const HOLD: Duration = Duration::from_secs(15);
/// The CPU figures are averaged over about this long.
const SMOOTH: f64 = 10.0;

/// One line of the process table: an owner, or a process under it at some depth.
enum Item {
    Owner(Owner),
    Proc { idx: usize, depth: usize },
}

impl Item {
    /// The selection key: the pid, and whether it is the owner's line.
    fn key(&self, work: &[Work]) -> (i32, bool) {
        match self {
            Item::Owner(o) => (o.pid, true),
            Item::Proc { idx, .. } => (work[*idx].proc.pid, false),
        }
    }
}

struct App {
    smc: Option<Smc>,
    config: Config,
    config_error: Option<String>,
    fans: Vec<(f64, f64)>,
    spin: Vec<f64>,
    power: Power,
    /// The adapter's rating, and the power it delivers and the battery takes (negative when
    /// it supplies), smoothed, in watts.
    rating: Option<u32>,
    watts: (Option<f64>, Option<f64>),
    status: Option<Status>,
    /// Every process guvnor could control, with its CPU.
    work: Vec<Work>,
    /// The table: busy owners, busiest first, each with its busy or controlled processes.
    items: Vec<Item>,
    cpu_prev: HashMap<i32, u64>,
    cpu_at: Instant,
    /// Each process's CPU, smoothed.
    cpu_avg: HashMap<i32, f64>,
    view: View,
    /// The last key press, for holding the order.
    pressed: Option<Instant>,
    /// Our own rpm samples, for the chart when no governor is running.
    local: VecDeque<Sample>,
    message: Option<(String, Instant)>,
    /// The selected line, kept by (pid, owner line) across refreshes.
    selected: Option<(i32, bool)>,
}

impl App {
    fn new() -> App {
        let mut app = App {
            smc: Smc::open(),
            config: Config::default(),
            config_error: None,
            fans: vec![],
            spin: vec![],
            power: Power::default(),
            rating: None,
            watts: (None, None),
            status: None,
            work: vec![],
            items: vec![],
            cpu_prev: HashMap::new(),
            cpu_at: Instant::now(),
            cpu_avg: HashMap::new(),
            view: View::default(),
            pressed: None,
            local: VecDeque::new(),
            message: None,
            selected: None,
        };
        app.refresh_fast();
        app.refresh_slow();
        app
    }

    fn rpm(&self) -> u32 {
        self.fans
            .iter()
            .map(|f| f.0.max(0.0) as u32)
            .max()
            .unwrap_or(0)
    }

    /// The target in effect: the governor's if it runs, else the config's.
    fn target(&self) -> u32 {
        self.status
            .as_ref()
            .map(|s| s.target)
            .unwrap_or_else(|| self.config.effective(self.power).0)
    }

    fn refresh_fast(&mut self) {
        if let Some(smc) = &self.smc {
            self.fans = smc.fans();
            // PDTR: delivered by the adapter; SBAP: drawn from the battery. They swing with
            // each burst of work, so smooth them over a few seconds.
            let a = 1.0 - (-0.5f64 / 5.0).exp();
            let ema = |old: Option<f64>, new: Option<f64>| match (old, new) {
                (Some(o), Some(n)) => Some(o + a * (n - o)),
                (_, n) => n,
            };
            self.watts = (
                ema(self.watts.0, smc.number(b"PDTR")),
                ema(self.watts.1, smc.number(b"SBAP").map(|w| -w)),
            );
        }
        self.spin.resize(self.fans.len(), 0.0);
        // Paused or running changes twice a period; refresh it more often than the CPU figures.
        for w in &mut self.work {
            if let Some(p) = procs::info(w.proc.pid) {
                w.proc.stopped = p.stopped;
            }
        }
        self.status = crate::running();
        let now = state::now();
        if self.local.back().is_none_or(|s| now - s.t >= 1.0) {
            if self.local.len() >= 3600 {
                self.local.pop_front();
            }
            self.local.push_back(Sample {
                t: now,
                rpm: self.rpm(),
                duty: 1.0,
                temp: None,
                setpoint: None,
                avg: None,
                load: state::load(),
                heat: None,
            });
        }
    }

    fn refresh_slow(&mut self) {
        match Config::load(&config::default_path()) {
            Ok(c) => {
                self.config = c;
                self.config_error = None;
            }
            Err(e) => self.config_error = Some(e),
        }
        self.power = Power::current();
        self.rating = crate::power::adapter_rating();
        let elapsed = self.cpu_at.elapsed().as_nanos().max(1) as f64;
        self.cpu_at = Instant::now();
        let mut prev = std::mem::take(&mut self.cpu_prev);
        let mut avg = std::mem::take(&mut self.cpu_avg);
        let a = 1.0 - (-elapsed / 1e9 / SMOOTH).exp();
        let work: Vec<Work> = procs::Matcher::new(&self.config.work)
            .classify()
            .into_iter()
            .map(|proc| {
                let ns = procs::cpu_ns(proc.pid).unwrap_or(0);
                let now = prev
                    .remove(&proc.pid)
                    .map(|old| ns.saturating_sub(old) as f64 / elapsed * 100.0);
                self.cpu_prev.insert(proc.pid, ns);
                // The first reading starts the average; until then there is none.
                let cpu = match (avg.remove(&proc.pid), now) {
                    (Some(old), Some(now)) => old + a * (now - old),
                    (None, Some(now)) => now,
                    (old, None) => old.unwrap_or(0.0),
                };
                self.cpu_avg.insert(proc.pid, cpu);
                Work { proc, cpu }
            })
            .collect();
        self.work = work;
        self.rebuild();
    }

    /// Lays out the table again, keeping the selection: if a selected process has gone, its
    /// owner is selected instead.
    fn rebuild(&mut self) {
        self.view.frozen = self.pressed.is_some_and(|t| t.elapsed() < HOLD);
        let owner_of_selected = match self.selected {
            Some((pid, false)) => self
                .work
                .iter()
                .find(|w| w.proc.pid == pid)
                .map(|w| w.proc.owner),
            _ => None,
        };
        self.items = items(&self.work, &self.config.work, self.selected, &mut self.view);
        let present = |k| self.items.iter().any(|i| i.key(&self.work) == k);
        if let Some(k) = self.selected
            && !present(k)
        {
            self.selected = owner_of_selected.map(|o| (o, true)).filter(|&k| present(k));
        }
    }

    /// Advances the fan spinners in proportion to their speed.
    fn tick(&mut self, dt: f64) {
        for (i, (rpm, _)) in self.fans.iter().enumerate() {
            self.spin[i] += rpm.max(0.0) / 60.0 * dt * 0.25;
        }
    }

    fn say(&mut self, msg: String) {
        self.message = Some((msg, Instant::now()));
    }

    fn toggle_pause(&mut self) {
        let result = if state::paused() {
            state::set_paused(false)
                .map(|_| "throttling on".to_string())
                .map_err(|e| e.to_string())
        } else {
            crate::pause(&self.config).map(|_| "throttling off; all work runs freely".to_string())
        };
        match result {
            Ok(m) => self.say(m),
            Err(e) => self.say(format!("error: {e}")),
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let k = self.selected?;
        self.items.iter().position(|i| i.key(&self.work) == k)
    }

    fn select(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let i = match self.selected_index() {
            Some(i) => (i as i32 + delta).clamp(0, self.items.len() as i32 - 1) as usize,
            None => 0,
        };
        self.selected = Some(self.items[i].key(&self.work));
    }

    /// → opens the selected owner; ← closes it, or the owner of the selected process.
    fn open(&mut self, open: bool) {
        let owner = match self.selected {
            Some((pid, true)) => pid,
            Some((pid, false)) => match self.work.iter().find(|w| w.proc.pid == pid) {
                Some(w) => w.proc.owner,
                None => return,
            },
            None => return,
        };
        if open {
            self.view.open.insert(owner);
        } else {
            self.view.open.remove(&owner);
            self.selected = Some((owner, true));
        }
        self.rebuild();
    }

    /// Enter: the selected owner or process moves to its next control state.
    fn cycle(&mut self) {
        let Some(i) = self.selected_index() else {
            self.say("select an owner or a process first (↑/↓)".into());
            return;
        };
        let path = config::default_path();
        let result = match &self.items[i] {
            Item::Owner(o) => apply_owner(o, &path, &self.config),
            Item::Proc { idx, .. } => {
                let proc = self.work[*idx].proc.clone();
                apply(next_choice(&proc), &proc, &path, &self.config)
            }
        };
        match result {
            Ok(m) => self.say(m),
            Err(e) => self.say(format!("error: {e}")),
        }
        self.refresh_slow();
    }

    fn nudge_target(&mut self, delta: i64) {
        let path = config::default_path();
        match set_target(&path, delta) {
            Ok(t) => {
                let when = if self.status.is_some() {
                    format!("applies within {} s", self.config.period)
                } else {
                    "applies when the governor starts".into()
                };
                self.say(format!("target {t} rpm saved to the config; {when}"));
                self.refresh_slow();
            }
            Err(e) => self.say(format!("error: {e}")),
        }
    }

    fn nudge_min_charge(&mut self, delta: i64) {
        match set_min_charge(&config::default_path(), delta) {
            Ok(min) => {
                self.say(match min {
                    Some(m) => format!("on AC, keep the battery at {m}% or more"),
                    None => "on AC, no battery minimum".into(),
                });
                self.refresh_slow();
            }
            Err(e) => self.say(format!("error: {e}")),
        }
    }
}

/// Moves [battery] min_charge_on_ac in the config by `delta` percent: from off it starts at 50,
/// it stays at or below 75 (macOS's own holds are at 80 or more), and below 10 it is off.
fn set_min_charge(path: &std::path::Path, delta: i64) -> Result<Option<u32>, String> {
    let mut min = None;
    edit_config(path, |doc| {
        let current = doc
            .get("battery")
            .and_then(|b| b.get("min_charge_on_ac"))
            .and_then(|v| v.as_integer());
        let new = match current {
            None => 50,
            Some(c) => (c + delta).min(75),
        };
        if new < 10 {
            if let Some(b) = doc.get_mut("battery").and_then(|b| b.as_table_like_mut()) {
                b.remove("min_charge_on_ac");
            }
            return Ok(());
        }
        min = Some(new as u32);
        let battery = doc
            .entry("battery")
            .or_insert(toml_edit::table())
            .as_table_like_mut()
            .ok_or("[battery] is not a table")?;
        match battery.get_mut("min_charge_on_ac") {
            Some(item) => set_value(item, new.into()),
            None => {
                battery.insert("min_charge_on_ac", toml_edit::value(new));
            }
        }
        Ok(())
    })?;
    Ok(min)
}

/// Edits the config file with toml_edit, keeping its comments and layout, and writes it only
/// if the result is a valid config. Starts from the commented defaults when there is no file.
fn edit_config(
    path: &std::path::Path,
    edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<(), String>,
) -> Result<Config, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => config::SAMPLE.to_string(),
        Err(e) => return Err(e.to_string()),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())?;
    edit(&mut doc)?;
    let out = doc.to_string();
    let config = Config::parse(&out)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, out).map_err(|e| e.to_string())?;
    Ok(config)
}

/// Replaces an item's value but keeps its decoration (the comment on its line).
fn set_value(item: &mut toml_edit::Item, value: toml_edit::Value) {
    match item.as_value_mut() {
        Some(v) => {
            let decor = v.decor().clone();
            *v = value;
            *v.decor_mut() = decor;
        }
        None => *item = toml_edit::Item::Value(value),
    }
}

/// Changes target_rpm in the config file.
fn set_target(path: &std::path::Path, delta: i64) -> Result<i64, String> {
    let mut target = 0;
    edit_config(path, |doc| {
        let current = doc
            .get("target_rpm")
            .and_then(|v| v.as_integer())
            .unwrap_or(Config::default().target_rpm as i64);
        target = (current + delta).clamp(0, 10_000);
        set_value(&mut doc["target_rpm"], target.into());
        Ok(())
    })?;
    Ok(target)
}

/// A whole-name rule covering this process and its kind, with a label for the menu. Cargo test
/// binaries end in a 16-hex-digit hash that changes with every build, and interpreters often
/// carry a version (python3.14), so both match any hash or version.
fn family_rule(name: &str) -> (String, String) {
    let n = name.len();
    if n > 17
        && name.as_bytes()[n - 17] == b'-'
        && name[n - 16..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        let base = &name[..n - 17];
        return (
            format!("{}-[0-9a-f]{{16}}", regex::escape(base)),
            format!("{base}-<hash>"),
        );
    }
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if base.len() < n && base.ends_with(|c: char| c.is_ascii_alphabetic()) {
        return (
            format!("{}[0-9.]*", regex::escape(base)),
            format!("{base}*"),
        );
    }
    (regex::escape(name), name.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Choice {
    /// Control just this process, until it exits.
    PinOn,
    /// Control every process of this kind (config).
    AllOn,
    /// Control neither: drop the per-process choice and the config rule for its kind.
    AllOff,
}

/// Enter cycles a process through: not controlled → just this process → all like it → not
/// controlled.
fn next_choice(p: &Proc) -> Choice {
    match p.reason {
        "" | "excluded" | "released" => Choice::PinOn,
        "pid" => Choice::AllOn,
        _ => Choice::AllOff,
    }
}

/// Carries out a choice: per-process ones in the state directory, the rest in [work] names and
/// exclude_names. Returns a message for the footer.
fn apply(
    choice: Choice,
    p: &Proc,
    path: &std::path::Path,
    current: &Config,
) -> Result<String, String> {
    let me = (p.pid, p.start);
    let mut pins = state::read_pins();
    pins.on.retain(|x| *x != me);
    pins.off.retain(|x| *x != me);
    if choice == Choice::PinOn {
        pins.on.push(me);
        state::write_pins(&pins).map_err(|e| e.to_string())?;
        return Ok(format!(
            "{} (pid {}): controlled, just this process",
            p.name, p.pid
        ));
    }
    // A config choice replaces any choice made for this process alone.
    state::write_pins(&pins).map_err(|e| e.to_string())?;
    let (rule, kind) = family_rule(&p.name);
    let mut work = current.work.clone();
    let still = |work: &config::Work, want: bool| {
        procs::Matcher::new(work)
            .classify()
            .iter()
            .any(|q| q.pid == p.pid && q.controlled() == want)
    };
    let msg = if choice == Choice::AllOn {
        work.exclude_names
            .retain(|e| *e != rule && *e != regex::escape(&p.name));
        if !still(&work, true) {
            if work
                .exclude_names
                .iter()
                .any(|e| Regex::new(&format!("^(?:{e})$")).is_ok_and(|r| r.is_match(&p.name)))
            {
                return Err(format!(
                    "{} is excluded by another pattern in exclude_names; edit the config",
                    p.name
                ));
            }
            work.names.push(rule.clone());
        }
        format!("{}: controlled, with all {kind} processes", p.name)
    } else {
        work.names
            .retain(|n| *n != rule && *n != regex::escape(&p.name));
        if still(&work, true) {
            work.exclude_names.push(rule.clone());
        }
        format!("{}: not controlled, nor any {kind} process", p.name)
    };
    edit_config(path, |doc| {
        if !doc.contains_table("work") {
            doc["work"] = toml_edit::table();
        }
        let to_array =
            |v: &[String]| toml_edit::Value::Array(v.iter().map(String::as_str).collect());
        set_value(&mut doc["work"]["names"], to_array(&work.names));
        set_value(
            &mut doc["work"]["exclude_names"],
            to_array(&work.exclude_names),
        );
        Ok(())
    })?;
    Ok(msg)
}

/// Groups the processes by owner, one line each. An owner is listed when its work is busy (5%
/// or more), when any of it is controlled, or when it is chosen. Under an opened owner go its
/// processes that are busy (1% or more) or controlled, as a tree with the idle ones between
/// them left out.
fn items(
    work: &[Work],
    rules: &config::Work,
    selected: Option<(i32, bool)>,
    view: &mut View,
) -> Vec<Item> {
    let now = Instant::now();
    view.busy_at.retain(|_, t| now.duration_since(*t) < LINGER);
    let mut lingers = |key: (i32, bool), busy: bool| {
        if busy {
            view.busy_at.insert(key, now);
        }
        view.busy_at.contains_key(&key)
    };
    let pins = state::read_pins();
    let owner_rule = (!rules.owners.is_empty())
        .then(|| Regex::new(&format!("^(?:{})$", rules.owners.join("|"))).ok())
        .flatten();
    let idx: HashMap<i32, usize> = work
        .iter()
        .enumerate()
        .map(|(i, w)| (w.proc.pid, i))
        .collect();
    let mut groups: HashMap<i32, Vec<usize>> = HashMap::new();
    for (i, w) in work.iter().enumerate() {
        groups.entry(w.proc.owner).or_default().push(i);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut owners: Vec<(Owner, Vec<usize>)> = vec![];
    for (pid, members) in groups {
        let total: f64 = members.iter().map(|&i| work[i].cpu.max(0.0)).sum();
        let Some(info) = idx
            .get(&pid)
            .map(|&i| work[i].proc.clone())
            .or_else(|| procs::info(pid))
        else {
            continue;
        };
        let name = procs::owner_name(&info, &procs::args);
        let choice = if pins.owners.contains(&(pid, info.start)) {
            "this"
        } else if owner_rule.as_ref().is_some_and(|r| r.is_match(&name)) {
            "all"
        } else {
            ""
        };
        let has_selected = selected.is_some_and(|(p, owner_line)| {
            if owner_line {
                p == pid
            } else {
                members.iter().any(|&i| work[i].proc.pid == p)
            }
        });
        let count = members.iter().filter(|&&i| work[i].proc.pid != pid).count();
        let members: Vec<usize> = members
            .into_iter()
            .filter(|&i| {
                let w = &work[i];
                lingers((w.proc.pid, false), w.cpu >= 1.0)
                    || w.proc.controlled()
                    || selected == Some((w.proc.pid, false))
            })
            .collect();
        if !(lingers((pid, true), total >= 5.0)
            || !choice.is_empty()
            || members.iter().any(|&i| work[i].proc.controlled())
            || has_selected)
        {
            continue;
        }
        // Apps run in /: say nothing rather than that.
        let place = procs::cwd(pid)
            .filter(|c| c != "/")
            .map(|c| match c.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => c,
            })
            .unwrap_or_default();
        let controlled = members
            .iter()
            .filter(|&&i| work[i].proc.controlled())
            .count();
        owners.push((
            Owner {
                pid,
                start: info.start,
                name,
                place,
                total,
                choice,
                controlled,
                count,
                open: view.open.contains(&pid),
            },
            members,
        ));
    }
    // Busiest first; while frozen, the order last shown with new lines last.
    let rank = |key: (i32, bool)| {
        if view.frozen {
            view.order.get(&key).copied().unwrap_or(usize::MAX)
        } else {
            0
        }
    };
    owners.sort_by(|a, b| {
        rank((a.0.pid, true))
            .cmp(&rank((b.0.pid, true)))
            .then(b.0.total.total_cmp(&a.0.total))
            .then(a.0.pid.cmp(&b.0.pid))
    });

    let mut out = vec![];
    for (owner, members) in owners {
        // Each shown process hangs from its nearest shown ancestor, or from the owner.
        let shown: HashMap<i32, usize> = members.iter().map(|&i| (work[i].proc.pid, i)).collect();
        let parent = |i: usize| -> i32 {
            let mut p = work[i].proc.ppid;
            let mut hops = 0;
            while hops < 64 && p != owner.pid && !shown.contains_key(&p) {
                match idx.get(&p) {
                    Some(&j) => p = work[j].proc.ppid,
                    None => return owner.pid,
                }
                hops += 1;
            }
            if shown.contains_key(&p) { p } else { owner.pid }
        };
        let mut kids: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut sub: HashMap<i32, f64> = HashMap::new();
        for &i in &members {
            let pid = work[i].proc.pid;
            // The owner itself is listed first among its own work.
            let up = if pid == owner.pid { 0 } else { parent(i) };
            kids.entry(up).or_default().push(i);
            let mut p = pid;
            let mut hops = 0;
            while p != owner.pid && p != 0 && hops < 64 {
                *sub.entry(p).or_default() += work[i].cpu.max(0.0);
                p = shown.get(&p).map(|&j| parent(j)).unwrap_or(owner.pid);
                hops += 1;
            }
        }
        let owner_pid = owner.pid;
        let open = owner.open;
        out.push(Item::Owner(owner));
        if !open {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![];
        let push = |stack: &mut Vec<(usize, usize)>, parent: i32, depth: usize| {
            let mut list = kids.get(&parent).cloned().unwrap_or_default();
            // Popped from the end: the busiest (or first, while frozen) goes last.
            list.sort_by(|&a, &b| {
                let s = |i: usize| sub.get(&work[i].proc.pid).copied().unwrap_or(0.0);
                let r = |i: usize| rank((work[i].proc.pid, false));
                r(b).cmp(&r(a))
                    .then(s(a).total_cmp(&s(b)))
                    .then(work[b].proc.pid.cmp(&work[a].proc.pid))
            });
            stack.extend(list.into_iter().map(|i| (i, depth)));
        };
        push(&mut stack, owner_pid, 0);
        push(&mut stack, 0, 0);
        while let Some((i, depth)) = stack.pop() {
            out.push(Item::Proc { idx: i, depth });
            let pid = work[i].proc.pid;
            if pid != owner_pid {
                push(&mut stack, pid, depth + 1);
            }
        }
    }
    view.order = out
        .iter()
        .enumerate()
        .map(|(n, i)| (i.key(work), n))
        .collect();
    out
}

/// Enter on an owner: not controlled → its work in this session → the work of every owner
/// with its name → not controlled.
fn apply_owner(o: &Owner, path: &std::path::Path, current: &Config) -> Result<String, String> {
    let me = (o.pid, o.start);
    let mut pins = state::read_pins();
    let (rule, kind) = family_rule(&o.name);
    let mut owners = current.work.owners.clone();
    let named = |r: &String| *r == rule || *r == regex::escape(&o.name);
    let msg = match o.choice {
        "" => {
            pins.owners.push(me);
            state::write_pins(&pins).map_err(|e| e.to_string())?;
            return Ok(format!(
                "work under {} (pid {}): controlled, this one only",
                o.name, o.pid
            ));
        }
        "this" => {
            pins.owners.retain(|x| *x != me);
            if !owners.iter().any(named) {
                owners.push(rule.clone());
            }
            format!("work under every {kind}: controlled")
        }
        _ => {
            pins.owners.retain(|x| *x != me);
            owners.retain(|r| !named(r));
            let left = Regex::new(&format!("^(?:{})$", owners.join("|")))
                .is_ok_and(|r| !owners.is_empty() && r.is_match(&o.name));
            if left {
                return Err(format!(
                    "{} matches another pattern in [work] owners; edit the config",
                    o.name
                ));
            }
            format!("work under {kind}: not controlled (rules by name still apply)")
        }
    };
    state::write_pins(&pins).map_err(|e| e.to_string())?;
    edit_config(path, |doc| {
        if !doc.contains_table("work") {
            doc["work"] = toml_edit::table();
        }
        let v = toml_edit::Value::Array(owners.iter().map(String::as_str).collect());
        set_value(&mut doc["work"]["owners"], v);
        Ok(())
    })?;
    Ok(msg)
}

pub fn run() -> Result<(), String> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result
}

/// Leaves the dashboard for nano on the config file (the commented defaults when there is
/// none), then comes back and says whether what was saved is valid: the governor ignores a
/// broken config and keeps its settings.
fn edit_in_nano(terminal: &mut DefaultTerminal, app: &mut App) -> DefaultTerminal {
    let path = config::default_path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, config::SAMPLE);
    }
    let _ = terminal.flush();
    ratatui::restore();
    let ran = std::process::Command::new("nano").arg(&path).status();
    let mut fresh = ratatui::init();
    let _ = fresh.clear();
    match (ran, Config::load(&path)) {
        (Err(e), _) => app.say(format!("error: cannot run nano: {e}")),
        (Ok(_), Err(e)) => app.say(format!(
            "config error, the governor keeps its settings: {e}"
        )),
        (Ok(_), Ok(_)) => app.say(format!(
            "config saved{}",
            if app.status.is_some() {
                format!("; applies within {} s", app.config.period)
            } else {
                String::new()
            }
        )),
    }
    app.refresh_slow();
    fresh
}

fn event_loop(terminal: &mut DefaultTerminal) -> Result<(), String> {
    let mut app = App::new();
    let (mut fast, mut slow, mut last) = (Instant::now(), Instant::now(), Instant::now());
    loop {
        terminal
            .draw(|f| draw(f, &app))
            .map_err(|e| e.to_string())?;
        if event::poll(Duration::from_millis(80)).map_err(|e| e.to_string())?
            && let Event::Key(k) = event::read().map_err(|e| e.to_string())?
            && k.kind == KeyEventKind::Press
        {
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Char('t') | KeyCode::Char('p') | KeyCode::Char(' ') => app.toggle_pause(),
                KeyCode::Char('+') | KeyCode::Char('=') => app.nudge_target(100),
                KeyCode::Char('-') => app.nudge_target(-100),
                KeyCode::Char(']') => app.nudge_min_charge(5),
                KeyCode::Char('[') => app.nudge_min_charge(-5),
                KeyCode::Char('e') => {
                    *terminal = edit_in_nano(terminal, &mut app);
                }
                KeyCode::Up | KeyCode::Char('k') => app.select(-1),
                KeyCode::Down | KeyCode::Char('j') => app.select(1),
                KeyCode::Right | KeyCode::Char('l') => app.open(true),
                KeyCode::Left | KeyCode::Char('h') => app.open(false),
                KeyCode::Enter => app.cycle(),
                _ => {}
            }
            // Hold the order while choosing, and show the effect of a key at once.
            app.pressed = Some(Instant::now());
            app.rebuild();
            app.refresh_fast();
            fast = Instant::now();
        }
        app.tick(last.elapsed().as_secs_f64());
        last = Instant::now();
        if fast.elapsed() >= Duration::from_millis(500) {
            app.refresh_fast();
            fast = Instant::now();
        }
        if slow.elapsed() >= Duration::from_secs(2) {
            app.refresh_slow();
            slow = Instant::now();
        }
    }
}

/// A colour key for a chart, as a second title: "━ rpm  ━ target …".
fn key_line(items: &[(&str, Color)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for &(name, color) in items {
        spans.push(Span::styled("━ ", Style::new().fg(color).bold()));
        spans.push(Span::styled(format!("{name}  "), Style::new().fg(DIM)));
    }
    Line::from(spans).right_aligned()
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).bold(),
        ))
}

/// The colour of a fan speed relative to the target: off, base, high, limit.
fn zone(rpm: u32, target: u32, c: &Config) -> (Color, &'static str) {
    if rpm < c.rules.off_below {
        (DIM, "off")
    } else if rpm > target + c.rules.limit_above {
        (HOT, "LIMIT")
    } else if rpm > target + c.rules.high_above {
        (WARN, "high")
    } else if rpm + c.rules.high_above < target {
        (OK, "below")
    } else {
        (OK, "at target")
    }
}

fn draw(f: &mut Frame, app: &App) {
    let [header, top, charts, bottom, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2 + 2 * app.fans.len().max(2) as u16),
        Constraint::Min(10),
        Constraint::Length(16),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, header, app);
    let [fans, duty] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
    draw_fans(f, fans, app);
    draw_duty(f, duty, app);
    draw_charts(f, charts, app);
    let [work, events] =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(bottom);
    draw_work(f, work, app);
    draw_events(f, events, app);
    draw_footer(f, footer, app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let pill = match &app.status {
        Some(s) if s.paused => Span::styled(
            " ○ THROTTLING OFF ",
            Style::new().fg(Color::Black).bg(WARN).bold(),
        ),
        Some(s) if s.phase == "full" => Span::styled(
            " ● FULL SPEED ",
            Style::new().fg(Color::Black).bg(OK).bold(),
        ),
        Some(_) => Span::styled(
            " ● GOVERNING ",
            Style::new().fg(Color::Black).bg(ACCENT).bold(),
        ),
        None => Span::styled(
            " ○ NOT RUNNING ",
            Style::new().fg(Color::White).bg(HOT).bold(),
        ),
    };
    let power = match (app.power.on_battery, app.power.charge) {
        (true, Some(c)) => format!("battery {c}%"),
        (true, None) => "battery".into(),
        (false, Some(c)) => format!("AC · {c}%"),
        (false, None) => "AC".into(),
    };
    let mut spans = vec![
        Span::styled(" ✻ guvnor ", Style::new().fg(ACCENT).bold()),
        pill,
        Span::styled(
            format!("  target {} rpm", app.target()),
            Style::new().bold(),
        ),
        Span::styled(format!("  ·  {power}"), Style::new().fg(DIM)),
    ];
    if let (false, Some(w)) = (app.power.on_battery, app.watts.0) {
        let of = app.rating.map(|r| format!(" of {r}")).unwrap_or_default();
        spans.push(Span::styled(
            format!("  ·  adapter {w:.0}{of} W"),
            Style::new().fg(DIM),
        ));
    }
    if let Some(w) = app.watts.1.filter(|_| app.power.charge.is_some()) {
        // Draining on AC: the adapter is not enough.
        let color = if w < -0.5 && !app.power.on_battery {
            WARN
        } else {
            DIM
        };
        spans.push(Span::styled(
            format!("  ·  battery {w:+.0} W"),
            Style::new().fg(color),
        ));
    }
    if let Some(m) = app.config.battery.as_ref().and_then(|b| b.min_charge_on_ac) {
        spans.push(Span::styled(
            format!("  ·  keep ≥ {m}%"),
            Style::new().fg(DIM),
        ));
    }
    if let Some(c) = app.status.as_ref().and_then(|s| s.ac_cap) {
        spans.push(Span::styled(
            format!("  adapter short: work limited to {:.0}%", c * 100.0),
            Style::new().fg(WARN),
        ));
    } else if let (Some(m), Some(c), Some(w), Some(s)) = (
        app.config.battery.as_ref().and_then(|b| b.min_charge_on_ac),
        app.power.charge,
        app.watts.1,
        &app.status,
    ) && s.paused
        && !app.power.on_battery
        && c <= m
        && w < -1.0
    {
        // The guard stands down while throttling is off: say why the battery is falling.
        spans.push(Span::styled(
            "  below the minimum, but throttling is off",
            Style::new().fg(WARN),
        ));
    }
    if let Some(s) = &app.status {
        spans.push(Span::styled(
            format!("  ·  pid {}  ·  period {} s", s.pid, s.period),
            Style::new().fg(DIM),
        ));
    }
    f.render_widget(Line::from(spans), area);
    let clock = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:02}:{:02}:{:02} ", tm.tm_hour, tm.tm_min, tm.tm_sec)
    };
    f.render_widget(
        Paragraph::new(clock).alignment(Alignment::Right).fg(DIM),
        area,
    );
}

fn draw_fans(f: &mut Frame, area: Rect, app: &App) {
    let block = panel("Fans");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.fans.is_empty() {
        f.render_widget(Paragraph::new("cannot read the SMC").fg(HOT), inner);
        return;
    }
    let target = app.target();
    let rows = Layout::vertical(vec![Constraint::Length(2); app.fans.len()]).split(inner);
    for (i, &(rpm, max)) in app.fans.iter().enumerate() {
        let rpm_u = rpm.max(0.0) as u32;
        let (color, label) = zone(rpm_u, target, &app.config);
        let glyph = if rpm_u < app.config.rules.off_below {
            "○"
        } else {
            SPINNER[app.spin[i] as usize % 4]
        };
        let [text, bar] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(rows[i]);
        f.render_widget(
            Line::from(vec![
                Span::styled(format!(" {glyph} "), Style::new().fg(color).bold()),
                Span::styled(format!("fan{i} "), Style::new().fg(DIM)),
                Span::styled(format!("{rpm_u:>5} rpm "), Style::new().fg(color).bold()),
                Span::styled(format!("{label:<5}"), Style::new().fg(color)),
                Span::styled(format!("  max {max:.0}"), Style::new().fg(DIM)),
            ]),
            text,
        );
        draw_fan_bar(f, bar, rpm, max.max(1.0), target, color);
    }
}

/// A bar from 0 to the fan's maximum, with the target marked.
fn draw_fan_bar(f: &mut Frame, area: Rect, rpm: f64, max: f64, target: u32, color: Color) {
    let area = Rect {
        x: area.x + 3,
        width: area.width.saturating_sub(4),
        ..area
    };
    let w = area.width as usize;
    if w == 0 {
        return;
    }
    let filled = ((rpm / max).clamp(0.0, 1.0) * w as f64).round() as usize;
    let mark = ((target as f64 / max).clamp(0.0, 1.0) * w as f64) as usize;
    let spans: Vec<Span> = (0..w)
        .map(|x| {
            if x == mark.min(w - 1) {
                Span::styled("┃", Style::new().fg(Color::White).bold())
            } else if x < filled {
                Span::styled("━", Style::new().fg(color))
            } else {
                Span::styled("─", Style::new().fg(Color::Rgb(48, 54, 61)))
            }
        })
        .collect();
    f.render_widget(Line::from(spans), area);
}

fn draw_duty(f: &mut Frame, area: Rect, app: &App) {
    let block = panel("Work allowed");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(s) = &app.status else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from("No governor running.").fg(DIM),
                Line::from(vec![
                    Span::raw("Start one with "),
                    Span::styled("guvnor run", Style::new().fg(ACCENT)),
                ]),
                Line::from(vec![
                    Span::raw("or "),
                    Span::styled("guvnor install", Style::new().fg(ACCENT)),
                ]),
            ]),
            inner,
        );
        return;
    };
    // How much the work gets to run, and one steady line saying why. Cycles are a second
    // long in temperature mode, too fast to show as a countdown.
    let [gauge, _, why] = Layout::vertical([Constraint::Length(1); 3]).areas(inner);
    let duty = if s.paused { 1.0 } else { s.duty };
    f.render_widget(
        Gauge::default()
            .ratio(duty.clamp(0.0, 1.0))
            .label(Span::styled(
                format!("{:.0}%", duty * 100.0),
                Style::new().fg(Color::White).bold(),
            ))
            .gauge_style(Style::new().fg(DUTY).bg(Color::Rgb(40, 34, 52)))
            .use_unicode(true),
        gauge,
    );
    let ceiling = if s.ceiling < 1.0 {
        format!(" · ceiling {:.0}%", s.ceiling * 100.0)
    } else {
        String::new()
    };
    let (text, color) = match (s.phase.as_str(), s.setpoint) {
        ("off", _) => ("throttling off · all work runs freely".to_string(), WARN),
        ("full", Some(sp)) => (format!("full speed · the die is under {sp:.0} °C"), OK),
        ("full", None) => ("full speed".to_string(), OK),
        ("stagger", _) => {
            let work: Vec<&Work> = app.work.iter().filter(|w| w.proc.controlled()).collect();
            let running = work.iter().filter(|w| !w.proc.stopped).count();
            (
                format!("staggered · {running} of {} running", work.len()),
                Color::Gray,
            )
        }
        (_, Some(sp)) => {
            let now = s
                .temp
                .map(|t| format!(" · now {t:.0} °C"))
                .unwrap_or_default();
            (format!("holding the die at {sp:.0} °C{now}"), Color::Gray)
        }
        (_, None) => match s.cap {
            Some(c) => (
                format!("fan rules · learned cap {:.0}%", c * 100.0),
                Color::Gray,
            ),
            None => ("fan rules".to_string(), Color::Gray),
        },
    };
    f.render_widget(
        Line::from(vec![
            Span::styled(text, Style::new().fg(color)),
            Span::styled(ceiling, Style::new().fg(DIM)),
        ]),
        why,
    );
}

fn draw_charts(f: &mut Frame, area: Rect, app: &App) {
    let [rpm_area, lower] =
        Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);
    let [duty_area, load_area, temp_area] = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(lower);
    let now = state::now();
    let (samples, title): (Vec<Sample>, &str) = match &app.status {
        Some(s) if !s.history.is_empty() => (s.history.clone(), "Fan speed"),
        _ => (app.local.iter().copied().collect(), "Fan speed (this view)"),
    };
    // The window grows with the history, from 5 minutes up to an hour.
    let oldest = samples.first().map(|s| now - s.t).unwrap_or(0.0);
    let span = (oldest / 300.0).ceil().clamp(1.0, 12.0) * 300.0;
    let target = app.target() as f64;
    let rules = &app.config.rules;
    let floor_rpm = (rules.off_below as f64).min(target) - 300.0;
    let peak = samples
        .iter()
        .map(|s| s.rpm as f64)
        .fold(target + rules.limit_above as f64 + 100.0, f64::max);
    let rpm_pts: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| (s.t - now, (s.rpm as f64).max(floor_rpm)))
        .collect();
    let top = (peak / 100.0).ceil() * 100.0;
    let line = |y: f64| vec![(-span, y), (0.0, y)];
    let (t_line, h_line, l_line) = (
        line(target),
        line(target + rules.high_above as f64),
        line(target + rules.limit_above as f64),
    );
    let x_axis = Axis::default()
        .bounds([-span, 0.0])
        .labels(
            [4.0, 3.0, 2.0, 1.0]
                .map(|q| format!("-{:.0}m", span * q / 4.0 / 60.0))
                .into_iter()
                .chain(["now".to_string()])
                .map(|l| Span::styled(l, Style::new().fg(DIM))),
        )
        .style(Style::new().fg(DIM));
    let rpm_chart = Chart::new(vec![
        Dataset::default()
            .name("limit")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(LIMIT_LINE))
            .data(&l_line),
        Dataset::default()
            .name("high")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(HIGH_LINE))
            .data(&h_line),
        Dataset::default()
            .name("target")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(TARGET))
            .data(&t_line),
        Dataset::default()
            .name("rpm")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(ACCENT))
            .data(&rpm_pts),
    ])
    .block(panel(title).title(key_line(&[
        ("rpm", ACCENT),
        ("target", TARGET),
        ("high", HIGH_LINE),
        ("limit", LIMIT_LINE),
    ])))
    .x_axis(x_axis.clone())
    .y_axis(
        Axis::default()
            .bounds([floor_rpm, top])
            .labels(
                // Labels sit evenly up the axis, so they name evenly spaced values.
                [
                    "off".to_string(),
                    format!("{:.0}", (floor_rpm + top) / 2.0),
                    format!("{top:.0}"),
                ]
                .map(|l| Span::styled(l, Style::new().fg(DIM))),
            )
            .style(Style::new().fg(DIM)),
    )
    .legend_position(None);
    f.render_widget(rpm_chart, rpm_area);

    let duty_pts: Vec<(f64, f64)> = match &app.status {
        Some(s) => s
            .history
            .iter()
            .map(|h| (h.t - now, h.duty * 100.0))
            .collect(),
        None => vec![],
    };
    let duty_chart = Chart::new(vec![
        Dataset::default()
            .name("duty %")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(DUTY))
            .data(&duty_pts),
    ])
    .block(panel("Duty").title(key_line(&[("average per 10 s", DUTY)])))
    .x_axis(short_axis(span))
    .y_axis(
        Axis::default()
            .bounds([0.0, 100.0])
            .labels(["0%", "50%", "100%"].map(|l| Span::styled(l, Style::new().fg(DIM))))
            .style(Style::new().fg(DIM)),
    )
    .legend_position(None);
    f.render_widget(duty_chart, duty_area);
    draw_load(f, load_area, &samples, now, span);
    draw_temps(f, temp_area, app, &samples, now, span);
}

/// A time axis with only its ends labelled, for the narrow charts.
fn short_axis(span: f64) -> Axis<'static> {
    Axis::default()
        .bounds([-span, 0.0])
        .labels(
            [format!("-{:.0}m", span / 60.0), "now".into()]
                .map(|l| Span::styled(l, Style::new().fg(DIM))),
        )
        .style(Style::new().fg(DIM))
}

/// The 1-minute load average, against the number of cores.
fn draw_load(f: &mut Frame, area: Rect, samples: &[Sample], now: f64, span: f64) {
    let cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as f64;
    let pts: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|s| s.load.map(|l| (s.t - now, l)))
        .collect();
    let cores_line = vec![(-span, cores), (0.0, cores)];
    let peak = pts.iter().map(|p| p.1).fold(cores, f64::max);
    let top = (peak * 1.1 / 5.0).ceil() * 5.0;
    let cores_key = format!("{cores:.0} cores");
    let block = panel("Load average").title(key_line(&[("1 min", LOAD), (&cores_key, DIM)]));
    let chart = Chart::new(vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(DIM))
            .data(&cores_line),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(LOAD))
            .data(&pts),
    ])
    .block(block)
    .x_axis(short_axis(span))
    .y_axis(
        Axis::default()
            .bounds([0.0, top])
            .labels(
                [0.0, top / 2.0, top]
                    .map(|v| Span::styled(format!("{v:.0}"), Style::new().fg(DIM))),
            )
            .style(Style::new().fg(DIM)),
    )
    .legend_position(None);
    f.render_widget(chart, area);
}

/// The hottest die reading in each period, and the setpoint the temperature loop held.
fn draw_temps(f: &mut Frame, area: Rect, app: &App, samples: &[Sample], now: f64, span: f64) {
    let temps: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|s| s.temp.map(|t| (s.t - now, t)))
        .collect();
    // The setpoint now, as one point at the right edge: a line of it hides behind the others.
    let setpoint = app
        .status
        .as_ref()
        .and_then(|s| s.setpoint)
        .or_else(|| samples.last().and_then(|s| s.setpoint));
    let set: Vec<(f64, f64)> = setpoint.map(|t| (0.0, t)).into_iter().collect();
    let set_key = setpoint
        .map(|t| format!("setpoint {t:.0}°"))
        .unwrap_or_else(|| "setpoint".into());
    let avg: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|s| s.avg.map(|t| (s.t - now, t)))
        .collect();
    // The latest readings, beside their lines.
    let latest = |now: Option<f64>, name: &str| {
        now.map(|t| format!("{name} {t:.0}°"))
            .unwrap_or_else(|| name.into())
    };
    let status = app.status.as_ref();
    let hot_key = latest(
        status
            .and_then(|s| s.temp)
            .or_else(|| samples.last().and_then(|s| s.temp)),
        "hottest",
    );
    let avg_key = latest(
        status
            .and_then(|s| s.avg)
            .or_else(|| samples.last().and_then(|s| s.avg)),
        "average",
    );
    let block = panel("Die temperature").title(key_line(&[
        (&hot_key, HOT),
        (&avg_key, AVG),
        (&set_key, TARGET),
    ]));
    if temps.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let why = match &app.status {
            Some(s) if s.control == "fans" => "Not used with control = \"fans\".",
            Some(_) => "Collecting…",
            None => "Appears while a governor runs.",
        };
        f.render_widget(Paragraph::new(why).fg(DIM), inner);
        return;
    }
    let vals = temps.iter().chain(&set).chain(&avg).map(|p| p.1);
    let lo = (vals.clone().fold(f64::MAX, f64::min) / 5.0).floor() * 5.0 - 5.0;
    let hi = (vals.fold(f64::MIN, f64::max) / 5.0).ceil() * 5.0 + 5.0;
    let chart = Chart::new(vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(AVG))
            .data(&avg),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(HOT))
            .data(&temps),
        Dataset::default()
            .marker(Marker::Block)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(TARGET))
            .data(&set),
    ])
    .block(block)
    .x_axis(
        Axis::default()
            .bounds([-span, 0.0])
            .labels(
                [format!("-{:.0}m", span / 60.0), "now".into()]
                    .map(|l| Span::styled(l, Style::new().fg(DIM))),
            )
            .style(Style::new().fg(DIM)),
    )
    .y_axis(
        Axis::default()
            .bounds([lo, hi])
            .labels(
                [lo, (lo + hi) / 2.0, hi]
                    .map(|v| Span::styled(format!("{v:.0}°"), Style::new().fg(DIM))),
            )
            .style(Style::new().fg(DIM)),
    )
    .legend_position(None);
    f.render_widget(chart, area);
}

fn draw_work(f: &mut Frame, area: Rect, app: &App) {
    let controlled: Vec<&Work> = app.work.iter().filter(|w| w.proc.controlled()).collect();
    let paused = controlled.iter().filter(|w| w.proc.stopped).count();
    let total: f64 = controlled.iter().map(|w| w.cpu.max(0.0)).sum();
    let title = format!(
        "Owners and processes · {} controlled · {paused} paused · {:.0}% CPU",
        controlled.len(),
        total
    );
    let block = panel(&title);
    if app.items.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(
                "Nothing busy. Busy work appears here under its owner (a Claude Code session, \
                 a command in a terminal tab, an app): select one with ↑/↓ and press Enter to \
                 control it.",
            )
            .wrap(ratatui::widgets::Wrap { trim: true })
            .fg(DIM),
            inner,
        );
        return;
    }
    let header = Row::new(
        ["PID", "STATE", "CPU", "CONTROL", "OWNER  └ ITS PROCESSES"]
            .map(|h| Cell::from(h).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD))),
    );
    let cpu_color = |cpu: f64| {
        if cpu >= 100.0 {
            HOT
        } else if cpu >= 20.0 {
            WARN
        } else {
            DIM
        }
    };
    // Controlled but left running: the governor does not pause work this idle.
    let idle = |w: &Work| !w.proc.stopped && w.cpu < app.config.work.idle_below;
    let mut owner = "";
    let rows: Vec<Row> = app
        .items
        .iter()
        .map(|item| match item {
            Item::Owner(o) => {
                owner = &o.name;
                let control = match o.choice {
                    "this" => "its work".to_string(),
                    "all" => format!("all {}", family_rule(&o.name).1),
                    _ => "—".to_string(),
                };
                let control = match (control.as_str(), o.controlled) {
                    ("—", n) if n > 0 => format!("{n} by rule"),
                    _ => control,
                };
                let on = !o.choice.is_empty();
                let mark = if o.open { "▾ " } else { "▸ " };
                let place = if o.place.is_empty() {
                    String::new()
                } else {
                    format!(" · in {}", o.place)
                };
                let runs = match o.count {
                    1 => "  owns 1 process".to_string(),
                    n => format!("  owns {n} processes"),
                };
                // Its controlled processes, read at each draw like the process lines: all
                // paused, all running, or some of each.
                let (n, stopped) = app
                    .work
                    .iter()
                    .filter(|w| w.proc.owner == o.pid && w.proc.controlled() && !idle(w))
                    .fold((0, 0), |(n, s), w| (n + 1, s + w.proc.stopped as usize));
                let (state, color) = match (n, stopped) {
                    (0, _) if o.controlled > 0 => ("◌ idle".to_string(), DIM),
                    (0, _) => ("○ free".to_string(), DIM),
                    (n, s) if s == n => ("⏸ paused".to_string(), WARN),
                    (_, 0) => ("▶ running".to_string(), OK),
                    (n, s) => (format!("⏸ {s}/{n}"), WARN),
                };
                Row::new([
                    Cell::from(o.pid.to_string()).style(Style::new().fg(DIM)),
                    Cell::from(state).style(Style::new().fg(color).bold()),
                    Cell::from(format!("{:>5.0}%", o.total))
                        .style(Style::new().fg(cpu_color(o.total)).bold()),
                    Cell::from(control).style(Style::new().fg(if on { ACCENT } else { DIM })),
                    Cell::from(Line::from(vec![
                        Span::styled(mark, Style::new().fg(ACCENT)),
                        Span::styled(o.name.clone(), Style::new().fg(ACCENT).bold()),
                        Span::styled(runs, Style::new().fg(Color::Gray)),
                        Span::styled(place, Style::new().fg(DIM)),
                    ])),
                ])
                .style(Style::new().bg(Color::Rgb(24, 32, 44)))
            }
            Item::Proc { idx, depth } => {
                let w = &app.work[*idx];
                let p = &w.proc;
                let (state, color) = match p.reason {
                    "" => ("○ free", DIM),
                    "excluded" | "released" => ("⊘ excluded", DIM),
                    _ if p.stopped => ("⏸ paused", WARN),
                    _ if idle(w) => ("◌ idle", DIM),
                    _ => ("▶ running", OK),
                };
                // What controls it: its owner, just this process, or a rule for its kind.
                let control = match p.reason {
                    "owner" => "owner".to_string(),
                    "pid" => "this".to_string(),
                    "" => "—".to_string(),
                    "excluded" => "excluded".to_string(),
                    "released" => "released".to_string(),
                    r => format!("all ({r})"),
                };
                let name = if p.controlled() { Color::White } else { DIM };
                let tree = if p.pid == p.owner {
                    "  · ".to_string()
                } else {
                    format!("  {}└ ", "  ".repeat(*depth))
                };
                let label = if p.pid == p.owner {
                    format!("{owner} itself")
                } else {
                    p.name.clone()
                };
                Row::new([
                    Cell::from(p.pid.to_string()).style(Style::new().fg(DIM)),
                    Cell::from(state).style(Style::new().fg(color)),
                    Cell::from(format!("{:>5.0}%", w.cpu)).style(Style::new().fg(cpu_color(w.cpu))),
                    Cell::from(control).style(Style::new().fg(if p.reason == "pid" {
                        ACCENT
                    } else {
                        DIM
                    })),
                    Cell::from(Line::from(vec![
                        Span::styled(tree, Style::new().fg(DIM)),
                        Span::styled(label, Style::new().fg(name)),
                    ])),
                ])
            }
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(11),
            Constraint::Length(7),
            Constraint::Length(13),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(
        Style::new()
            .bg(Color::Rgb(38, 52, 70))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▸")
    .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);
    let mut state = ratatui::widgets::TableState::default().with_selected(app.selected_index());
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_events(f: &mut Frame, area: Rect, app: &App) {
    let block = panel("Events");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines: Vec<Line> = vec![];
    if let Some(e) = &app.config_error {
        lines.push(Line::from(Span::styled(
            format!("config: {e}"),
            Style::new().fg(HOT),
        )));
    }
    if let Some(s) = &app.status {
        let n = inner.height as usize;
        for e in s.events.iter().rev().take(n).rev() {
            // "YYYY-MM-DD HH:MM:SS message": show the time only.
            let (time, msg) = e.get(11..19).zip(e.get(20..)).unwrap_or(("", e.as_str()));
            let color = if msg.contains("(Limit)") || msg.contains("error") {
                HOT
            } else if msg.contains("(High)")
                || msg.starts_with("throttling off")
                || msg.starts_with("watchdog")
            {
                WARN
            } else if msg.contains("duty") && msg.contains("->") {
                OK
            } else if msg.starts_with("config") || msg.starts_with("power") {
                ACCENT
            } else {
                Color::Gray
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{time} "), Style::new().fg(DIM)),
                Span::styled(msg.to_string(), Style::new().fg(color)),
            ]));
        }
    } else {
        lines.push(Line::from("Events appear here while a governor runs.").fg(DIM));
    }
    let skip = lines.len().saturating_sub(inner.height as usize);
    f.render_widget(Paragraph::new(lines.split_off(skip)), inner);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let key = |k: &'static str| {
        Span::styled(
            format!(" {k} "),
            Style::new().fg(Color::Black).bg(ACCENT).bold(),
        )
    };
    let pause = if state::paused() {
        " throttling on  "
    } else {
        " throttling off  "
    };
    let mut spans = vec![
        key("↑↓"),
        Span::raw(" select  "),
        key("→←"),
        Span::raw(" open  "),
        key("⏎"),
        Span::raw(" control: off → this → all  "),
        key("t"),
        Span::raw(pause),
        key("+/-"),
        Span::raw(" target  "),
        key("[/]"),
        Span::raw(" battery minimum  "),
        key("e"),
        Span::raw(" edit config  "),
        key("q"),
        Span::raw(" quit  "),
    ];
    if let Some((m, at)) = &app.message
        && at.elapsed() < Duration::from_secs(6)
    {
        spans.push(Span::styled(format!("  {m}"), Style::new().fg(WARN)));
    }
    f.render_widget(Line::from(spans), area);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The tests that point GUVNOR_STATE somewhere take turns.
    pub(crate) static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn set_target_keeps_comments_and_validates() {
        let dir = std::env::temp_dir().join(format!("guvnor-tui-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        // No file yet: starts from the commented sample.
        assert_eq!(set_target(&path, 100).unwrap(), 2600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("target_rpm = 2600") && text.contains("# keep the fastest fan"));
        assert_eq!(set_target(&path, -300).unwrap(), 2300);
        assert_eq!(Config::load(&path).unwrap().target_rpm, 2300);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn min_charge_starts_at_50_stays_below_80_and_turns_off() {
        let dir = std::env::temp_dir().join(format!("guvnor-charge-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let min = |c: &Config| c.battery.as_ref().and_then(|b| b.min_charge_on_ac);
        // From the sample: starts at 50, and sets nothing else.
        assert_eq!(set_min_charge(&path, 5).unwrap(), Some(50));
        let c = Config::load(&path).unwrap();
        assert_eq!(
            (min(&c), c.battery.as_ref().unwrap().max_duty),
            (Some(50), None)
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# keep the fastest fan")
        );
        for _ in 0..10 {
            set_min_charge(&path, 5).unwrap();
        }
        assert_eq!(set_min_charge(&path, 5).unwrap(), Some(75));
        // Down past 10: off.
        for _ in 0..13 {
            set_min_charge(&path, -5).unwrap();
        }
        assert_eq!(set_min_charge(&path, -5).unwrap(), None);
        assert_eq!(min(&Config::load(&path).unwrap()), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn family_rules_cover_versions_and_cargo_hashes() {
        assert_eq!(family_rule("python3.14").0, "python[0-9.]*");
        assert_eq!(
            family_rule("sheets-c15f623685212d0e").0,
            "sheets-[0-9a-f]{16}"
        );
        assert_eq!(family_rule("rustc").0, "rustc");
        assert_eq!(family_rule("cc1plus").0, "cc1plus");
        assert_eq!(family_rule("x86_64").0, "x86_64");
        let r = Regex::new(&format!("^(?:{})$", family_rule("python3.14").0)).unwrap();
        assert!(r.is_match("python3") && r.is_match("python3.12") && !r.is_match("pythonw"));
    }

    #[test]
    fn enter_on_an_owner_cycles_off_this_all_off() {
        let _env = ENV.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("guvnor-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        unsafe {
            std::env::set_var("GUVNOR_STATE", &dir);
        }
        // This test process stands in for the owner: it is alive, so its pin is kept.
        let me = procs::info(std::process::id() as i32).unwrap();
        let mut o = Owner {
            pid: me.pid,
            start: me.start,
            name: "claude".into(),
            place: String::new(),
            total: 0.0,
            choice: "",
            controlled: 0,
            count: 0,
            open: false,
        };
        let mut seen = vec![];
        for _ in 0..3 {
            let c = Config::load(&cfg).unwrap();
            apply_owner(&o, &cfg, &c).unwrap();
            let c = Config::load(&cfg).unwrap();
            o.choice = if state::read_pins().owners.contains(&(me.pid, me.start)) {
                "this"
            } else if c.work.owners.contains(&"claude".to_string()) {
                "all"
            } else {
                ""
            };
            seen.push(o.choice);
        }
        assert_eq!(seen, vec!["this", "all", ""]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn enter_cycles_off_this_all_off() {
        let _env = ENV.lock().unwrap();
        // A scratch config and state, and a sleep that is not our child (our descendants are
        // protected from throttling, so they never count as work).
        let dir = std::env::temp_dir().join(format!("guvnor-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        unsafe {
            std::env::set_var("GUVNOR_CONFIG", &cfg);
            std::env::set_var("GUVNOR_STATE", &dir);
        }
        let tag = format!("7541.{}", std::process::id());
        std::process::Command::new("/bin/sh")
            .args(["-c", &format!("/bin/sleep {tag} &")])
            .status()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let out = std::process::Command::new("pgrep")
            .args(["-f", &format!("^/bin/sleep {tag}$")])
            .output()
            .unwrap();
        let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        let now = || {
            let c = Config::load(&cfg).unwrap();
            let p = procs::Matcher::new(&c.work)
                .classify()
                .into_iter()
                .find(|p| p.pid == pid)
                .unwrap();
            (c, p)
        };
        let mut seen = vec![];
        for _ in 0..4 {
            let (c, p) = now();
            seen.push(p.reason);
            apply(next_choice(&p), &p, &cfg, &c).unwrap();
        }
        assert_eq!(seen, vec!["", "pid", "name", ""]);
        // The config edits kept the file's comments, and left names as they were.
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# keep the fastest fan"));
        assert_eq!(
            Config::load(&cfg).unwrap().work.names,
            Config::default().work.names
        );
        unsafe { libc::kill(pid, libc::SIGKILL) };
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
