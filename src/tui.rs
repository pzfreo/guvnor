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
            .find()
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
        work.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(a.proc.pid.cmp(&b.proc.pid)));
        self.work = work;
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

/// Changes target_rpm in the config file, keeping its comments and layout.
fn set_target(path: &std::path::Path, delta: i64) -> Result<i64, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => config::SAMPLE.to_string(),
        Err(e) => return Err(e.to_string()),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())?;
    let current = doc
        .get("target_rpm")
        .and_then(|v| v.as_integer())
        .unwrap_or(Config::default().target_rpm as i64);
    let target = (current + delta).clamp(0, 10_000);
    // Keep the line's comment: replace the value, not its decoration.
    match doc.get_mut("target_rpm").and_then(|i| i.as_value_mut()) {
        Some(v) => {
            let decor = v.decor().clone();
            *v = target.into();
            *v.decor_mut() = decor;
        }
        None => doc["target_rpm"] = toml_edit::value(target),
    }
    let out = doc.to_string();
    Config::parse(&out)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, out).map_err(|e| e.to_string())?;
    Ok(target)
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
                KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Up => app.nudge_target(100),
                KeyCode::Char('-') | KeyCode::Down => app.nudge_target(-100),
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
    } else {
        (OK, "base")
    }
}

fn draw(f: &mut Frame, app: &App) {
    let [header, top, charts, bottom, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2 + 2 * app.fans.len().max(2) as u16),
        Constraint::Min(10),
        Constraint::Length(12),
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
        (Some(t), Some(sp), _) => format!("die {t:.0} °C, holding {sp:.0} °C · {} s cycle", s.cycle),
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
        let run_w = (s.duty * w as f64).round() as usize;
        let elapsed = match s.phase.as_str() {
            "run" => s.duty * s.period - left,
            _ => s.period - left,
        };
        let now_x = ((elapsed / s.period).clamp(0.0, 1.0) * (w - 1) as f64) as usize;
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
    let [rpm_area, duty_area] =
        Layout::vertical([Constraint::Percentage(68), Constraint::Percentage(32)]).areas(area);
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
                [
                    "off".to_string(),
                    format!("{target:.0}"),
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
    .block(panel("Duty").title(key_line(&[("share of each period the work runs", DUTY)])))
    .x_axis(x_axis)
    .y_axis(
        Axis::default()
            .bounds([0.0, 100.0])
            .labels(["0%", "50%", "100%"].map(|l| Span::styled(l, Style::new().fg(DIM))))
            .style(Style::new().fg(DIM)),
    )
    .legend_position(None);
    f.render_widget(duty_chart, duty_area);
}

fn draw_work(f: &mut Frame, area: Rect, app: &App) {
    let paused = app.work.iter().filter(|w| w.proc.stopped).count();
    let total: f64 = app.work.iter().map(|w| w.cpu).sum();
    let title = format!(
        "Work · {} processes · {paused} paused · {:.0}% CPU",
        app.work.len(),
        total
    );
    let header = Row::new(
        ["PID", "STATE", "CPU", "MATCH", "NAME"]
            .map(|h| Cell::from(h).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD))),
    );
    let rows: Vec<Row> = app
        .work
        .iter()
        .map(|w| {
            let (state, color) = if w.proc.stopped {
                ("⏸ paused", WARN)
            } else {
                ("▶ running", OK)
            };
            let cpu_color = if w.cpu >= 100.0 {
                HOT
            } else if w.cpu >= 20.0 {
                WARN
            } else {
                DIM
            };
            Row::new([
                Cell::from(w.proc.pid.to_string()).style(Style::new().fg(DIM)),
                Cell::from(state).style(Style::new().fg(color)),
                Cell::from(format!("{:>5.0}%", w.cpu)).style(Style::new().fg(cpu_color)),
                Cell::from(w.proc.reason).style(Style::new().fg(DIM)),
                Cell::from(w.proc.name.clone()).style(Style::new().fg(Color::White)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(panel(&title));
    if app.work.is_empty() {
        let block = panel(&title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new("No work processes. Builds and tests appear here as they start.")
                .fg(DIM),
            inner,
        );
    } else {
        f.render_widget(table, area);
    }
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
}
