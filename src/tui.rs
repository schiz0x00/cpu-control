//! Interactive terminal UI. Changes are *staged* (shown in yellow) and only
//! written to the system when you press `w`.

use crate::cpuset::CpuSet;
use crate::ctx::Ctx;
use crate::plan::{Request, Tune, fmt_khz};
use crate::profile::{self, Profile, ProfileFile};
use crate::topology::{CoreKind, Globals, Topology};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState};
use std::collections::BTreeMap;
use std::time::Duration;

const FREQ_STEP_KHZ: u64 = 100_000;

#[derive(Clone, Default)]
struct Staged {
    governor: Option<String>,
    max_khz: Option<u64>,
}

struct App<'a> {
    ctx: &'a Ctx,
    topo: Topology,
    globals: Globals,
    desired: CpuSet,
    turbo: Option<bool>,
    staged: BTreeMap<usize, Staged>,
    preset: Option<Profile>,
    table: TableState,
    popup: Option<(Vec<(String, Profile)>, ListState)>,
    msg: String,
    quit: bool,
}

pub fn run(ctx: &Ctx, topo: Topology, globals: Globals) -> Result<()> {
    let mut app = App {
        ctx,
        desired: topo.online(),
        topo,
        globals,
        turbo: None,
        staged: BTreeMap::new(),
        preset: None,
        table: TableState::default().with_selected(0),
        popup: None,
        msg: if ctx.dry_run { "DRY RUN: nothing will be written".into() } else { String::new() },
        quit: false,
    };
    let mut terminal = ratatui::init();
    let res = app.event_loop(&mut terminal);
    ratatui::restore();
    res
}

impl App<'_> {
    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(1000))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        self.on_key(k.code, k.modifiers);
                    }
                }
            } else {
                // Idle tick: refresh live frequencies without disturbing staged edits.
                if let Ok((t, g)) = self.ctx.discover() {
                    self.topo = t;
                    self.globals = g;
                }
            }
        }
        Ok(())
    }

    fn cursor(&self) -> usize {
        self.table.selected().unwrap_or(0).min(self.topo.cpus.len().saturating_sub(1))
    }

    fn cursor_cpu(&self) -> &crate::topology::Cpu {
        &self.topo.cpus[self.cursor()]
    }

    fn dirty(&self) -> bool {
        self.desired != self.topo.online() || self.turbo.is_some() || !self.staged.is_empty() || self.preset.is_some()
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.topo.cpus.len() as isize;
        let i = (self.cursor() as isize + delta).rem_euclid(n);
        self.table.select(Some(i as usize));
    }

    /// Stage a group on/off. If every changeable member is already wanted
    /// online it goes offline, otherwise everything goes online.
    fn toggle_group(&mut self, group: CpuSet, what: &str) {
        let changeable: Vec<usize> = group.iter().filter(|&i| self.topo.get(i).is_some_and(|c| c.hotpluggable)).collect();
        if changeable.is_empty() {
            self.msg = format!("{what}: nothing here can be toggled");
            return;
        }
        let all_on = changeable.iter().all(|&i| self.desired.contains(i));
        for &i in &changeable {
            if all_on { self.desired.remove(i) } else { self.desired.insert(i) }
        }
        if self.desired.is_empty() {
            for &i in &changeable {
                self.desired.insert(i);
            }
            self.msg = "at least one CPU must stay online".into();
            return;
        }
        self.msg = format!("{what}: {} {} CPU(s)", if all_on { "offlining" } else { "onlining" }, changeable.len());
    }

    fn kind_set(&self, k: CoreKind) -> CpuSet {
        self.topo.of_kind(k)
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if self.popup.is_some() {
            return self.on_popup_key(code);
        }
        let cur = self.cursor_cpu().clone();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Home => self.table.select(Some(0)),
            KeyCode::End => self.table.select(Some(self.topo.cpus.len() - 1)),
            KeyCode::Char(' ') | KeyCode::Enter => {
                let one: CpuSet = [cur.id].into_iter().collect();
                self.toggle_group(one, &format!("cpu{}", cur.id));
            }
            KeyCode::Char('c') => self.toggle_group(
                self.topo.cpus.iter().filter(|c| c.package == cur.package && c.core_id == cur.core_id).map(|c| c.id).collect(),
                &format!("core {}", cur.core_id),
            ),
            KeyCode::Char('l') => self.toggle_group(cur.l2_group.clone(), "L2 cluster"),
            KeyCode::Char('p') => self.toggle_group(self.kind_set(CoreKind::P), "P-cores"),
            KeyCode::Char('e') => self.toggle_group(self.kind_set(CoreKind::E), "E-cores"),
            KeyCode::Char('o') => self.toggle_group(self.kind_set(CoreKind::LpE), "LP-E cores"),
            KeyCode::Char('s') => self.toggle_group(self.topo.smt_secondaries(), "SMT threads"),
            KeyCode::Char('a') => {
                self.desired = self.topo.all();
                self.msg = "all CPUs staged online".into();
            }
            KeyCode::Char('t') => {
                let now_on = self.globals.no_turbo.map(|nt| !nt);
                match now_on {
                    None => self.msg = "no turbo control on this system".into(),
                    Some(on) => {
                        let want = !self.turbo.unwrap_or(on);
                        self.turbo = if want == on { None } else { Some(want) };
                        self.msg = format!("turbo staged {}", if want { "on" } else { "off" });
                    }
                }
            }
            KeyCode::Char('g') => self.cycle_governor(cur.kind),
            KeyCode::Char(']') => self.step_max(cur.kind, FREQ_STEP_KHZ as i64),
            KeyCode::Char('[') => self.step_max(cur.kind, -(FREQ_STEP_KHZ as i64)),
            KeyCode::Char('P') | KeyCode::Char('m') => self.open_presets(),
            KeyCode::Char('u') => self.reset(),
            KeyCode::Char('w') => self.apply(),
            KeyCode::Char('r') => {
                self.refresh();
                self.msg = "refreshed".into();
            }
            _ => {}
        }
    }

    fn refresh(&mut self) {
        match self.ctx.discover() {
            Ok((t, g)) => {
                self.topo = t;
                self.globals = g;
            }
            Err(e) => self.msg = format!("refresh failed: {e}"),
        }
    }

    fn reset(&mut self) {
        self.desired = self.topo.online();
        self.turbo = None;
        self.staged.clear();
        self.preset = None;
        self.msg = "staged changes discarded".into();
    }

    fn group_of_kind(&self, k: CoreKind) -> Vec<usize> {
        self.topo.of_kind(k).iter().filter(|&i| self.desired.contains(i)).collect()
    }

    fn cycle_governor(&mut self, kind: CoreKind) {
        let govs = &self.globals.available_governors;
        if govs.is_empty() {
            self.msg = "no governors available".into();
            return;
        }
        let ids = self.group_of_kind(kind);
        let Some(&first) = ids.first() else { return };
        let current = self.staged.get(&first).and_then(|s| s.governor.clone()).or_else(|| self.topo.get(first).and_then(|c| c.governor.clone()));
        let idx = current.and_then(|g| govs.iter().position(|x| *x == g)).map_or(0, |i| (i + 1) % govs.len());
        let g = govs[idx].clone();
        for id in ids {
            self.staged.entry(id).or_default().governor = Some(g.clone());
        }
        self.msg = format!("{kind} cores: governor staged -> {g}");
    }

    fn step_max(&mut self, kind: CoreKind, delta: i64) {
        let ids = self.group_of_kind(kind);
        for &id in &ids {
            let Some(c) = self.topo.get(id) else { continue };
            let base = self.staged.get(&id).and_then(|s| s.max_khz).or(c.scaling_max_khz).unwrap_or(0) as i64;
            let lo = c.min_khz.unwrap_or(100_000) as i64;
            let hi = c.max_khz.unwrap_or(i64::MAX as u64) as i64;
            let new = (base + delta).clamp(lo, hi) as u64;
            self.staged.entry(id).or_default().max_khz = Some(new);
        }
        if let Some(&id) = ids.first() {
            let v = self.staged[&id].max_khz.unwrap_or(0);
            self.msg = format!("{kind} cores: max freq staged -> {}", fmt_khz(v));
        }
    }

    fn open_presets(&mut self) {
        let file = ProfileFile::load(&self.ctx.profiles_path).unwrap_or_default();
        let mut list: Vec<(String, Profile)> =
            profile::builtin().into_iter().filter(|(n, _)| !file.profile.contains_key(*n)).map(|(n, p)| (n.to_string(), p)).collect();
        list.extend(file.profile.into_iter());
        let mut st = ListState::default();
        st.select(Some(0));
        self.popup = Some((list, st));
    }

    fn on_popup_key(&mut self, code: KeyCode) {
        let Some((list, state)) = self.popup.as_mut() else { return };
        let n = list.len();
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.popup = None,
            KeyCode::Down | KeyCode::Char('j') => state.select(Some((state.selected().unwrap_or(0) + 1) % n)),
            KeyCode::Up | KeyCode::Char('k') => state.select(Some((state.selected().unwrap_or(0) + n - 1) % n)),
            KeyCode::Enter => {
                let (name, p) = list[state.selected().unwrap_or(0)].clone();
                self.popup = None;
                match profile::to_request(&self.topo, &p) {
                    Ok(req) => {
                        if p.online.is_some() || p.offline.is_some() {
                            let mut want = req.online.clone();
                            for c in self.topo.cpus.iter().filter(|c| !c.hotpluggable) {
                                want.insert(c.id);
                            }
                            self.desired = want;
                        }
                        self.turbo = req.turbo;
                        self.staged.clear();
                        self.preset = Some(p);
                        self.msg = format!("preset `{name}` staged; press w to apply, u to discard");
                    }
                    Err(e) => self.msg = format!("preset `{name}`: {e}"),
                }
            }
            _ => {}
        }
    }

    fn build_request(&self) -> Result<Request> {
        // Start from the staged preset (for turbo / governor defaults), but pin
        // the online set to what's shown on screen.
        let mut p = self.preset.clone().unwrap_or_default();
        p.online = Some(self.desired.to_string());
        p.offline = None;
        if self.turbo.is_some() {
            p.turbo = self.turbo;
        }
        let mut req = profile::to_request(&self.topo, &p)?;
        for (id, s) in &self.staged {
            req.tune.push(Tune {
                cpus: [*id].into_iter().collect(),
                governor: s.governor.clone(),
                max_khz: s.max_khz,
                ..Default::default()
            });
        }
        Ok(req)
    }

    fn apply(&mut self) {
        if !self.dirty() {
            self.msg = "nothing staged".into();
            return;
        }
        let result = self.build_request().and_then(|req| self.ctx.execute(&self.topo, &req));
        match result {
            Ok(o) if o.dry_run => self.msg = format!("dry run: {} action(s) would be applied", o.actions.len()),
            Ok(o) => {
                self.msg = if o.failures.is_empty() {
                    format!("applied {} change(s)", o.applied)
                } else {
                    let hint = o.hint.clone().unwrap_or_default();
                    format!("{} applied, {} failed: {} {hint}", o.applied, o.failures.len(), o.failures[0])
                };
                if let Some(w) = o.warnings.first() {
                    self.msg.push_str(&format!(" (warning: {w})"));
                }
                self.refresh();
                self.desired = self.topo.online();
                self.turbo = None;
                self.staged.clear();
                self.preset = None;
            }
            Err(e) => self.msg = format!("error: {e}"),
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [main, summary, help, status] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let header = Row::new(["CPU", "TYPE", "CORE", "THR", "L2", "STATE", "CUR", "MAX", "GOVERNOR", "EPP"])
            .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan));
        let rows: Vec<Row> = self
            .topo
            .cpus
            .iter()
            .map(|c| {
                let want = self.desired.contains(c.id);
                let (label, color) = match (c.online, want) {
                    (true, true) if !c.hotpluggable => ("fixed", Color::Gray),
                    (true, true) => ("online", Color::Green),
                    (false, false) => ("OFFLINE", Color::Red),
                    (true, false) => ("-> off", Color::Yellow),
                    (false, true) => ("-> on", Color::Yellow),
                };
                let st = self.staged.get(&c.id);
                let gov = match st.and_then(|s| s.governor.clone()) {
                    Some(g) => format!("{g}*"),
                    None => c.governor.clone().unwrap_or_else(|| "-".into()),
                };
                let max = match st.and_then(|s| s.max_khz) {
                    Some(m) => format!("{}*", fmt_khz(m)),
                    None => c.scaling_max_khz.map(fmt_khz).unwrap_or_else(|| "-".into()),
                };
                let thr = c.siblings.iter().position(|s| s == c.id).map(|i| format!("T{i}")).unwrap_or_default();
                let l2 = c.l2_group.iter().next().map(|x| format!("#{x}")).unwrap_or_default();
                let kind_color = match c.kind {
                    CoreKind::P => Color::Magenta,
                    CoreKind::E => Color::Blue,
                    CoreKind::LpE => Color::LightCyan,
                };
                let cell = |t: String| Cell::new(t);
                Row::new(vec![
                    cell(c.id.to_string()),
                    cell(c.kind.to_string()).style(Style::new().fg(kind_color)),
                    cell(if c.topology_known { c.core_id.to_string() } else { "?".into() }),
                    cell(thr),
                    cell(l2),
                    cell(label.to_string()).style(Style::new().fg(color).add_modifier(Modifier::BOLD)),
                    cell(c.cur_khz.map(fmt_khz).unwrap_or_else(|| "-".into())),
                    cell(max).style(if st.is_some_and(|s| s.max_khz.is_some()) { Style::new().fg(Color::Yellow) } else { Style::new() }),
                    cell(gov).style(if st.is_some_and(|s| s.governor.is_some()) { Style::new().fg(Color::Yellow) } else { Style::new() }),
                    cell(c.epp.clone().unwrap_or_else(|| "-".into())),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Min(8),
        ];
        let title = format!(" cpu-control — {} ", if self.dirty() { "STAGED CHANGES (w to apply)" } else { "live" });
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(title))
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(table, main, &mut self.table);

        // Summary of the *staged* configuration.
        let mut parts = Vec::new();
        for k in [CoreKind::P, CoreKind::E, CoreKind::LpE] {
            let all = self.topo.of_kind(k);
            if !all.is_empty() {
                parts.push(format!("{k}: {}/{}", all.intersection(&self.desired).len(), all.len()));
            }
        }
        let turbo = self.turbo.or(self.globals.no_turbo.map(|n| !n));
        let line = format!(
            "target online {}/{} ({})   {}   turbo: {}{}   smt: {}",
            self.desired.len(),
            self.topo.cpus.len(),
            parts.join("  "),
            self.desired,
            turbo.map_or("n/a", |t| if t { "on" } else { "off" }),
            if self.turbo.is_some() { "*" } else { "" },
            self.globals.smt_control.as_deref().unwrap_or("n/a"),
        );
        f.render_widget(Paragraph::new(line), summary);

        f.render_widget(
            Paragraph::new(vec![
                Line::from("↑↓/jk move  space toggle cpu  c core  l L2 cluster  p P-cores  e E-cores  o LP-E  s SMT threads  a all"),
                Line::from("m presets  t turbo  g governor(kind)  [ ] max freq(kind)  w apply  u discard  r refresh  q quit"),
            ])
            .block(Block::default().borders(Borders::TOP)),
            help,
        );
        f.render_widget(Paragraph::new(Span::styled(self.msg.clone(), Style::new().fg(Color::Yellow))), status);

        if let Some((list, state)) = self.popup.as_mut() {
            let area = centered(f.area(), 70, (list.len() as u16 + 2).min(20));
            f.render_widget(Clear, area);
            let items: Vec<ListItem> = list
                .iter()
                .map(|(n, p)| {
                    let d = p.description.clone().unwrap_or_else(|| profile::describe(p));
                    ListItem::new(format!("{n:<14} {d}"))
                })
                .collect();
            let w = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" presets (Enter stage, Esc cancel) "))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(w, area, state);
        }
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}
