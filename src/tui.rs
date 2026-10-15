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
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const ACCENT: Color = Color::Rgb(94, 196, 255);
const DIM: Color = Color::Rgb(110, 118, 129);
const OK: Color = Color::Rgb(87, 211, 140);
const WARN: Color = Color::Rgb(255, 196, 61);
const HOT: Color = Color::Rgb(255, 99, 99);
const DUTY: Color = Color::Rgb(199, 146, 255);

const TARGET: Color = Color::Rgb(70, 140, 90);
const HIGH_LINE: Color = Color::Rgb(150, 115, 30);
const LIMIT_LINE: Color = Color::Rgb(150, 55, 55);

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

struct Work {
    proc: Proc,
    cpu: f64,
}

struct App {
    smc: Option<Smc>,
    config: Config,
    config_error: Option<String>,
    fans: Vec<(f64, f64)>,
    spin: Vec<f64>,
    power: Power,
    status: Option<Status>,
    work: Vec<Work>,
    cpu_prev: HashMap<i32, u64>,
    cpu_at: Instant,
    /// Our own rpm samples, for the chart when no governor is running.
    local: VecDeque<Sample>,
    message: Option<(String, Instant)>,
    /// The selected process, kept by pid across refreshes.
    selected: Option<i32>,
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
            status: None,
            work: vec![],
            cpu_prev: HashMap::new(),
            cpu_at: Instant::now(),
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
        let elapsed = self.cpu_at.elapsed().as_nanos().max(1) as f64;
        self.cpu_at = Instant::now();
        let mut prev = std::mem::take(&mut self.cpu_prev);
        let mut work: Vec<Work> = procs::Matcher::new(&self.config.work)
            .classify()
            .into_iter()
            .map(|proc| {
                let ns = procs::cpu_ns(proc.pid).unwrap_or(0);
                let cpu = prev
                    .remove(&proc.pid)
                    .map(|old| ns.saturating_sub(old) as f64 / elapsed * 100.0);
                self.cpu_prev.insert(proc.pid, ns);
                Work {
                    proc,
                    cpu: cpu.unwrap_or(0.0),
                }
            })
            .collect();
        // Controlled processes first, then busy ones guvnor ignores (candidates), then the ones
        // left alone on purpose; busiest first within each.
        let group = |w: &Work| match w.proc.reason {
            "excluded" | "released" => 2,
            "" => 1,
            _ => 0,
        };
        let sel = self.selected;
        work.retain(|w| !w.proc.reason.is_empty() || w.cpu >= 5.0 || Some(w.proc.pid) == sel);
        work.sort_by(|a, b| {
            group(a)
                .cmp(&group(b))
                .then(b.cpu.total_cmp(&a.cpu))
                .then(a.proc.pid.cmp(&b.proc.pid))
        });
        self.work = work;
        if self
            .selected
            .is_some_and(|pid| !self.work.iter().any(|w| w.proc.pid == pid))
        {
            self.selected = None;
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
            crate::pause(&self.config).map(|_| "throttling off; all work continued".to_string())
        };
        match result {
            Ok(m) => self.say(m),
            Err(e) => self.say(format!("error: {e}")),
        }
    }

    fn select(&mut self, delta: i32) {
        if self.work.is_empty() {
            return;
        }
        let i = self
            .selected
            .and_then(|pid| self.work.iter().position(|w| w.proc.pid == pid));
        let i = match i {
            Some(i) => (i as i32 + delta).clamp(0, self.work.len() as i32 - 1) as usize,
            None => 0,
        };
        self.selected = Some(self.work[i].proc.pid);
    }

    /// Enter: the selected process moves to its next control state.
    fn cycle(&mut self) {
        let Some(w) = self
            .selected
            .and_then(|pid| self.work.iter().find(|w| w.proc.pid == pid))
        else {
            self.say("select a process first (↑/↓)".into());
            return;
        };
        let proc = w.proc.clone();
        match apply(
            next_choice(&proc),
            &proc,
            &config::default_path(),
            &self.config,
        ) {
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

pub fn run() -> Result<(), String> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result
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
                KeyCode::Char('p') | KeyCode::Char(' ') => app.toggle_pause(),
                KeyCode::Char('+') | KeyCode::Char('=') => app.nudge_target(100),
                KeyCode::Char('-') => app.nudge_target(-100),
                KeyCode::Up | KeyCode::Char('k') => app.select(-1),
                KeyCode::Down | KeyCode::Char('j') => app.select(1),
                KeyCode::Enter => app.cycle(),
                _ => {}
            }
            // Show the effect of a key at once.
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
fn key_line(items: &[(&'static str, Color)]) -> Line<'static> {
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
        Constraint::Length(14),
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
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(bottom);
    draw_work(f, work, app);
    draw_events(f, events, app);
    draw_footer(f, footer, app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let pill = match &app.status {
        Some(s) if s.paused => {
            Span::styled(" ⏸ PAUSED ", Style::new().fg(Color::Black).bg(WARN).bold())
        }
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
    let block = panel("Duty cycle");
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
    let [gauge, cap, phase, timeline] = Layout::vertical([Constraint::Length(1); 4]).areas(inner);
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
    let cap_text = match (s.temp, s.setpoint, s.cap) {
        (Some(t), Some(sp), _) => {
            format!("die {t:.0} °C, holding {sp:.0} °C · {} s cycle", s.cycle)
        }
        (_, Some(sp), _) => format!("holding {sp:.0} °C · {} s cycle", s.cycle),
        (_, _, Some(c)) => format!("learned cap {:.0}%", c * 100.0),
        _ => "no cap learned".into(),
    };
    f.render_widget(
        Line::from(vec![
            Span::styled(cap_text, Style::new().fg(DIM)),
            Span::styled(
                format!("  ·  ceiling {:.0}%", s.ceiling * 100.0),
                Style::new().fg(DIM),
            ),
        ]),
        cap,
    );
    let left = (s.phase_end - state::now()).max(0.0);
    let (icon, text, color) = match s.phase.as_str() {
        "run" => ("▶", format!("work running · pause in {left:.1} s"), OK),
        "pause" => ("⏸", format!("work paused · resumes in {left:.1} s"), WARN),
        "full" => ("▶", "work at full speed · nothing paused".into(), OK),
        "stagger" => {
            let work: Vec<&Work> = app.work.iter().filter(|w| w.proc.controlled()).collect();
            let running = work.iter().filter(|w| !w.proc.stopped).count();
            (
                "▶",
                format!(
                    "staggered: each runs {:.0}% of every {} s · {running} of {} running",
                    s.duty * 100.0,
                    s.cycle,
                    work.len()
                ),
                OK,
            )
        }
        _ => ("⏸", "throttling off (guvnor resume or p)".into(), WARN),
    };
    f.render_widget(
        Line::from(Span::styled(
            format!("{icon} {text}"),
            Style::new().fg(color).bold(),
        )),
        phase,
    );
    // This period as a strip: run part, pause part, and where we are now.
    let w = timeline.width as usize;
    if w > 0 && (s.phase == "run" || s.phase == "pause") {
        let cycle = if s.cycle > 0.0 { s.cycle } else { s.period };
        let run_w = (s.duty * w as f64).round() as usize;
        let elapsed = match s.phase.as_str() {
            "run" => s.duty * cycle - left,
            _ => cycle - left,
        };
        let now_x = ((elapsed / cycle).clamp(0.0, 1.0) * (w - 1) as f64) as usize;
        let spans: Vec<Span> = (0..w)
            .map(|x| {
                let c = if x < run_w { OK } else { WARN };
                if x == now_x {
                    Span::styled("◆", Style::new().fg(Color::White).bold())
                } else {
                    Span::styled("▬", Style::new().fg(c))
                }
            })
            .collect();
        f.render_widget(Line::from(spans), timeline);
    }
}

fn draw_charts(f: &mut Frame, area: Rect, app: &App) {
    let [rpm_area, lower] =
        Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);
    let [duty_area, temp_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(lower);
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
    let top = (peak / 100.0).ceil() * 100.0;
    let rpm_pts: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| (s.t - now, (s.rpm as f64).max(floor_rpm)))
        .collect();
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
    .x_axis(x_axis)
    .y_axis(
        Axis::default()
            .bounds([0.0, 100.0])
            .labels(["0%", "50%", "100%"].map(|l| Span::styled(l, Style::new().fg(DIM))))
            .style(Style::new().fg(DIM)),
    )
    .legend_position(None);
    f.render_widget(duty_chart, duty_area);
    draw_temps(f, temp_area, app, &samples, now, span);
}

/// The hottest die reading in each period, and the setpoint the temperature loop held.
fn draw_temps(f: &mut Frame, area: Rect, app: &App, samples: &[Sample], now: f64, span: f64) {
    let temps: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|s| s.temp.map(|t| (s.t - now, t)))
        .collect();
    let set: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|s| s.setpoint.map(|t| (s.t - now, t)))
        .collect();
    let block = panel("Die temperature").title(key_line(&[("hottest", HOT), ("setpoint", TARGET)]));
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
    let vals = temps.iter().chain(&set).map(|p| p.1);
    let lo = (vals.clone().fold(f64::MAX, f64::min) / 5.0).floor() * 5.0 - 5.0;
    let hi = (vals.fold(f64::MIN, f64::max) / 5.0).ceil() * 5.0 + 5.0;
    let chart = Chart::new(vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(TARGET))
            .data(&set),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(HOT))
            .data(&temps),
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
    let total: f64 = controlled
        .iter()
        .map(|w| w.cpu.max(0.0))
        .sum::<f64>()
        .max(0.0);
    let title = format!(
        "Processes · {} controlled · {paused} paused · {:.0}% CPU",
        controlled.len(),
        total
    );
    let block = panel(&title);
    if app.work.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(
                "Nothing busy. Processes using CPU appear here: select one with ↑/↓ and press \
                 Enter to control it.",
            )
            .wrap(ratatui::widgets::Wrap { trim: true })
            .fg(DIM),
            inner,
        );
        return;
    }
    let header = Row::new(
        ["PID", "STATE", "CPU", "CONTROL", "NAME"]
            .map(|h| Cell::from(h).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD))),
    );
    let rows: Vec<Row> = app
        .work
        .iter()
        .map(|w| {
            let p = &w.proc;
            let (state, color) = match p.reason {
                "" => ("○ free", DIM),
                "excluded" | "released" => ("⊘ excluded", DIM),
                _ if p.stopped => ("⏸ paused", WARN),
                _ => ("▶ running", OK),
            };
            // What controls it: just this process, or the config rule that catches its kind.
            let control = match p.reason {
                "pid" => "this".to_string(),
                "" => "—".to_string(),
                "excluded" => "excluded".to_string(),
                "released" => "released".to_string(),
                r => format!("all ({r})"),
            };
            let cpu_color = if w.cpu >= 100.0 {
                HOT
            } else if w.cpu >= 20.0 {
                WARN
            } else {
                DIM
            };
            let name = if p.controlled() { Color::White } else { DIM };
            Row::new([
                Cell::from(p.pid.to_string()).style(Style::new().fg(DIM)),
                Cell::from(state).style(Style::new().fg(color)),
                Cell::from(format!("{:>5.0}%", w.cpu)).style(Style::new().fg(cpu_color)),
                Cell::from(control).style(Style::new().fg(if p.reason == "pid" {
                    ACCENT
                } else {
                    DIM
                })),
                Cell::from(p.name.clone()).style(Style::new().fg(name)),
            ])
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
    .highlight_symbol("▸");
    let mut state = ratatui::widgets::TableState::default().with_selected(
        app.selected
            .and_then(|pid| app.work.iter().position(|w| w.proc.pid == pid)),
    );
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
                || msg.starts_with("paused")
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
        " resume  "
    } else {
        " pause  "
    };
    let mut spans = vec![
        key("↑↓"),
        Span::raw(" select  "),
        key("⏎"),
        Span::raw(" control: off → this → all  "),
        key("p"),
        Span::raw(pause),
        key("+/-"),
        Span::raw(" target  "),
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
mod tests {
    use super::*;

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
    fn enter_cycles_off_this_all_off() {
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
