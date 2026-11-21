//! Interactive terminal UI. Changes are *staged* (shown in yellow) and only
//! written to the system when you press `w`.

use crate::cpuset::CpuSet;
use crate::ctx::{self, Ctx};
use crate::plan::{Request, Tune, fmt_khz};
use crate::profile::{self, Profile, ProfileFile};
use crate::topology::{CoreKind, Globals, Topology};
use anyhow::{Context, Result};
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
    let mut app = App::new(ctx, topo, globals);
    if !ctx.dry_run && !ctx::is_root() {
        app.msg = "not root: w (apply) will be refused by the kernel; restart with sudo to make changes".into();
    }
    let mut terminal = ratatui::try_init().context("cannot set up the terminal (is this an interactive tty?)")?;
    let res = app.event_loop(&mut terminal);
    ratatui::restore();
    res
}

impl<'a> App<'a> {
    fn new(ctx: &'a Ctx, topo: Topology, globals: Globals) -> App<'a> {
        App {
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
        }
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(1000))? {
                if let Event::Key(k) = event::read()?
                    && k.kind == KeyEventKind::Press
                {
                    self.on_key(k.code, k.modifiers);
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
            // `siblings` is the physical core (and just this CPU while its
            // topology is unknown, when core_id is only a placeholder).
            KeyCode::Char('c') => {
                let what = if cur.topology_known { format!("core {}", cur.core_id) } else { format!("cpu{}", cur.id) };
                self.toggle_group(cur.siblings.clone(), &what);
            }
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
        let mut shown = None;
        for &id in &ids {
            let Some(c) = self.topo.get(id) else { continue };
            // A CPU that is offline right now has no live limit: step from its
            // hardware maximum, and leave it alone if even that is unknown.
            let Some(base) = self.staged.get(&id).and_then(|s| s.max_khz).or(c.scaling_max_khz).or(c.max_khz) else { continue };
            // Never below the current minimum: the kernel would not accept it.
            let lo = c.min_khz.unwrap_or(FREQ_STEP_KHZ).max(c.scaling_min_khz.unwrap_or(0));
            let hi = c.max_khz.unwrap_or(u64::MAX).max(lo);
            let new = base.saturating_add_signed(delta).clamp(lo, hi);
            self.staged.entry(id).or_default().max_khz = Some(new);
            shown.get_or_insert(new);
        }
        self.msg = match shown {
            Some(v) => format!("{kind} cores: max freq staged -> {}", fmt_khz(v)),
            None => format!("{kind} cores: no frequency limits to adjust"),
        };
    }

    fn open_presets(&mut self) {
        let file = ProfileFile::load(&self.ctx.profiles_path).unwrap_or_else(|e| {
            self.msg = format!("saved profiles unavailable ({e:#}); showing built-ins only");
            ProfileFile::default()
        });
        let list = profile::available(file);
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
                // After a failure keep what is staged, so it can be retried
                // rather than re-entered.
                if o.failures.is_empty() {
                    self.desired = self.topo.online();
                    self.turbo = None;
                    self.staged.clear();
                    self.preset = None;
                }
            }
            Err(e) => self.msg = format!("error: {e:#}"),
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
                let kind = if c.topology_known { c.kind.to_string() } else { "?".into() };
                let cell = |t: String| Cell::new(t);
                Row::new(vec![
                    cell(c.id.to_string()),
                    cell(kind).style(Style::new().fg(kind_color)),
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
            let area = centered(f.area(), 70, list.len().min(18) as u16 + 2);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Fake, fake_sys};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn cpus(s: &str) -> CpuSet {
        CpuSet::parse_list(s).unwrap()
    }

    fn app(ctx: &Ctx) -> App<'_> {
        let (topo, globals) = ctx.discover().unwrap();
        App::new(ctx, topo, globals)
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    /// Put the cursor on a CPU (rows are in id order on the fake machine).
    fn at(app: &mut App, id: usize) {
        app.table.select(Some(id));
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        buf.content.chunks(width as usize).map(|row| row.iter().map(|c| c.symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    /// A fake machine whose topology is already cached, plus contexts over it.
    fn setup() -> (Fake, Ctx, Ctx) {
        let f = fake_sys();
        let live = Ctx::for_test(&f, false);
        live.discover().unwrap();
        let dry = Ctx::for_test(&f, true);
        (f, live, dry)
    }

    #[test]
    fn starts_with_nothing_staged() {
        let (_f, live, dry) = setup();
        let a = app(&live);
        assert!(!a.dirty() && a.msg.is_empty() && !a.quit && a.popup.is_none());
        assert_eq!((a.cursor(), a.cursor_cpu().id), (0, 0));
        assert_eq!(a.desired, cpus("0-21"));
        assert!(app(&dry).msg.contains("DRY RUN"));
    }

    #[test]
    fn cursor_moves_and_wraps() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        a.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.cursor(), 21);
        a.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.cursor(), 0);
        press(&mut a, "jjjk");
        assert_eq!(a.cursor(), 2);
        a.on_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(a.cursor(), 21);
        a.on_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(a.cursor(), 0);
        assert!(!a.dirty());
        // A stale selection past the end is clamped rather than indexed.
        a.table.select(Some(500));
        assert_eq!((a.cursor(), a.cursor_cpu().id), (21, 21));
    }

    #[test]
    fn space_and_enter_toggle_the_cpu_under_the_cursor() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 3);
        press(&mut a, " ");
        assert_eq!(a.desired, cpus("0-2,4-21"));
        assert!(a.dirty() && a.msg.contains("cpu3: offlining 1"));
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(a.desired, cpus("0-21"));
        assert!(!a.dirty() && a.msg.contains("cpu3: onlining 1"));
    }

    #[test]
    fn the_fixed_cpu_cannot_be_staged_offline() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        press(&mut a, " ");
        assert_eq!(a.desired, cpus("0-21"));
        assert!(!a.dirty() && a.msg.contains("nothing here can be toggled"));
        // Its core still toggles, minus the fixed thread.
        press(&mut a, "c");
        assert_eq!(a.desired, cpus("0-4,6-21"));
    }

    #[test]
    fn group_keys_toggle_core_cluster_kinds_and_smt() {
        let (_f, live, _) = setup();
        for (cursor, key, gone, what) in [
            (1, 'c', "1-2", "core 8"),
            (13, 'l', "12-15", "L2 cluster"),
            (0, 'p', "1-11", "P-cores"),
            (0, 'e', "12-19", "E-cores"),
            (0, 'o', "20-21", "LP-E cores"),
            (0, 's', "2,4-5,7,9,11", "SMT threads"),
        ] {
            let mut a = app(&live);
            at(&mut a, cursor);
            press(&mut a, &key.to_string());
            assert_eq!(a.desired, cpus("0-21").difference(&cpus(gone)), "key {key}");
            assert!(a.msg.starts_with(what) && a.msg.contains("offlining"), "key {key}: {}", a.msg);
            press(&mut a, &key.to_string());
            assert_eq!(a.desired, cpus("0-21"), "key {key} twice");
        }
    }

    #[test]
    fn a_partly_offline_group_goes_online_first() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 13);
        press(&mut a, " e");
        assert_eq!(a.desired, cpus("0-21"));
        assert!(a.msg.contains("onlining 8"));
        press(&mut a, "ea");
        assert_eq!(a.desired, cpus("0-21"));
        assert!(a.msg.contains("all CPUs staged online"));
    }

    #[test]
    fn the_last_cpu_cannot_be_staged_offline() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        a.topo.cpus[0].hotpluggable = true; // a machine whose boot CPU can go
        press(&mut a, "eo");
        assert_eq!(a.desired, cpus("0-11"));
        press(&mut a, "p");
        assert_eq!(a.desired, cpus("0-11"), "the group that would empty the set is left as it was");
        assert!(a.msg.contains("at least one CPU must stay online"));
    }

    #[test]
    fn turbo_toggles_relative_to_the_live_state() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        press(&mut a, "t");
        assert_eq!(a.turbo, Some(false));
        assert!(a.dirty() && a.msg.contains("turbo staged off"));
        press(&mut a, "t");
        assert_eq!(a.turbo, None, "back at the live value: nothing staged");
        assert!(!a.dirty());
        a.globals.no_turbo = None;
        press(&mut a, "t");
        assert!(a.turbo.is_none() && a.msg.contains("no turbo control"));
    }

    #[test]
    fn governor_cycles_for_the_kind_under_the_cursor() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 14);
        press(&mut a, "g");
        assert_eq!(a.staged.keys().copied().collect::<CpuSet>(), cpus("12-19"));
        assert!(a.staged.values().all(|s| s.governor.as_deref() == Some("performance") && s.max_khz.is_none()));
        assert!(a.msg.contains("E cores: governor staged -> performance"));
        press(&mut a, "g");
        assert!(a.staged.values().all(|s| s.governor.as_deref() == Some("powersave")));

        // CPUs staged offline are not tuned.
        let mut a = app(&live);
        at(&mut a, 20);
        press(&mut a, " ");
        at(&mut a, 21);
        press(&mut a, "g");
        assert_eq!(a.staged.keys().copied().collect::<Vec<_>>(), [21]);

        a.globals.available_governors.clear();
        press(&mut a, "g");
        assert!(a.msg.contains("no governors available"));
    }

    #[test]
    fn max_frequency_steps_within_the_hardware_range() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 12);
        press(&mut a, "]");
        assert!(a.staged.values().all(|s| s.max_khz == Some(3_800_000)), "already at the top");
        press(&mut a, "[[");
        assert_eq!(a.staged.keys().copied().collect::<CpuSet>(), cpus("12-19"));
        assert!(a.staged.values().all(|s| s.max_khz == Some(3_600_000) && s.governor.is_none()));
        assert!(a.msg.contains("E cores: max freq staged -> 3.60 GHz"));
        // All the way down stops at the hardware minimum.
        for _ in 0..60 {
            press(&mut a, "[");
        }
        assert!(a.staged.values().all(|s| s.max_khz == Some(400_000)));
    }

    #[test]
    fn stepping_never_goes_below_the_live_minimum() {
        let (f, live, _) = setup();
        f.write("cpu20/cpufreq/scaling_min_freq", "2450000");
        let mut a = app(&live);
        at(&mut a, 20);
        press(&mut a, "[[[");
        assert_eq!(a.staged[&20].max_khz, Some(2_450_000));
        assert_eq!(a.staged[&21].max_khz, Some(2_200_000));
    }

    #[test]
    fn a_cpu_staged_online_steps_from_its_hardware_maximum() {
        // An offline CPU has no live limit. Stepping used to start from zero,
        // which staged the *minimum* frequency as the new maximum.
        let (f, live, _) = setup();
        f.offline(12);
        let mut a = app(&live);
        press(&mut a, "a");
        at(&mut a, 13);
        press(&mut a, "[");
        assert_eq!(a.staged[&12].max_khz, Some(3_700_000));
        assert_eq!(a.staged[&13].max_khz, Some(3_700_000));
    }

    #[test]
    fn a_cpu_with_no_known_limits_is_left_out_of_stepping() {
        let f = fake_sys();
        f.offline(12);
        let ctx = Ctx::for_test(&f, false); // no cache yet: cpu12 is unknown
        let mut a = app(&ctx);
        assert!(!a.topo.get(12).unwrap().topology_known);
        press(&mut a, "a");
        at(&mut a, 12);
        // Guessed to be a P-core, with no hardware limits on record.
        press(&mut a, "[");
        assert!(!a.staged.contains_key(&12));
        assert_eq!(a.staged[&1].max_khz, Some(4_700_000));

        // Nothing at all to step: say so instead of indexing a missing entry.
        let mut a = app(&ctx);
        for c in &mut a.topo.cpus {
            (c.max_khz, c.scaling_max_khz) = (None, None);
        }
        press(&mut a, "]");
        assert!(a.staged.is_empty() && a.msg.contains("no frequency limits"));
    }

    #[test]
    fn core_key_on_an_unknown_cpu_does_not_drag_in_a_real_core() {
        // Unknown cpu12 carries the placeholder core_id 12, the real core_id
        // of cpu3/cpu4. `c` must toggle cpu12 alone.
        let f = fake_sys();
        f.offline(12);
        let ctx = Ctx::for_test(&f, false);
        let mut a = app(&ctx);
        at(&mut a, 12);
        press(&mut a, "c");
        assert_eq!(a.desired, cpus("0-21"));
        assert!(a.msg.starts_with("cpu12: onlining 1"), "{}", a.msg);
        at(&mut a, 3);
        press(&mut a, "c");
        assert_eq!(a.desired, cpus("0-2,5-21"));
    }

    #[test]
    fn preset_popup_navigates_and_stages() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        press(&mut a, "m");
        let (list, state) = a.popup.as_ref().unwrap();
        assert_eq!((list.len(), state.selected()), (11, Some(0)));

        // Keys go to the popup while it is open.
        press(&mut a, "k");
        assert_eq!(a.popup.as_ref().unwrap().1.selected(), Some(10));
        press(&mut a, "j");
        a.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.popup.as_ref().unwrap().1.selected(), Some(1));
        press(&mut a, "p ");
        assert!(a.popup.is_some() && !a.dirty());

        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(a.popup.is_none());
        assert_eq!(a.desired, cpus("0,12-21"), "no-pcores keeps the fixed cpu0");
        assert!(a.dirty() && a.preset.is_some() && a.msg.contains("preset `no-pcores` staged"));

        // q/Esc close the popup without quitting or staging.
        press(&mut a, "Pq");
        assert!(a.popup.is_none() && !a.quit);
        press(&mut a, "m");
        a.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(a.popup.is_none() && !a.quit);
    }

    #[test]
    fn staging_a_preset_replaces_earlier_tuning_and_takes_its_turbo() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 12);
        press(&mut a, "g[tm");
        assert!(!a.staged.is_empty() && a.turbo == Some(false));
        for _ in 0..9 {
            press(&mut a, "j"); // max-perf
        }
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(a.staged.is_empty());
        assert_eq!(a.turbo, Some(true));
        assert_eq!(a.desired, cpus("0-1,3,6,8,10"));
        let req = a.build_request().unwrap();
        assert_eq!((req.online.clone(), req.turbo), (cpus("0-1,3,6,8,10"), Some(true)));
        assert_eq!(req.tune[0].governor.as_deref(), Some("performance"));
    }

    #[test]
    fn saved_profiles_join_the_list_and_a_broken_file_is_reported() {
        let (_f, live, _) = setup();
        std::fs::write(&live.profiles_path, "[profile.mine]\nonline = \"e\"\n").unwrap();
        let mut a = app(&live);
        press(&mut a, "m");
        let names: Vec<&str> = a.popup.as_ref().unwrap().0.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!((names.len(), names[11]), (12, "mine"));
        press(&mut a, "q");

        std::fs::write(&live.profiles_path, "[profile.mine\n").unwrap();
        press(&mut a, "m");
        assert_eq!(a.popup.as_ref().unwrap().0.len(), 11);
        assert!(a.msg.contains("saved profiles unavailable") && a.msg.contains("profiles.toml"), "{}", a.msg);
    }

    #[test]
    fn a_preset_that_cannot_apply_here_is_reported_not_staged() {
        let (_f, live, _) = setup();
        std::fs::write(&live.profiles_path, "[profile.aaa]\nonline = \"30-40\"\n").unwrap();
        let mut a = app(&live);
        press(&mut a, "mk"); // wraps to the last entry: the saved profile
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!a.dirty() && a.msg.starts_with("preset `aaa`:"), "{}", a.msg);
    }

    #[test]
    fn discard_drops_everything_staged() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 13);
        press(&mut a, " gt[m");
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        press(&mut a, "s");
        assert!(a.dirty());
        press(&mut a, "u");
        assert!(!a.dirty() && a.staged.is_empty() && a.turbo.is_none() && a.preset.is_none());
        assert_eq!(a.desired, cpus("0-21"));
        assert!(a.msg.contains("discarded"));
    }

    #[test]
    fn build_request_pins_the_online_set_and_appends_staged_tuning() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 20);
        press(&mut a, "eg[t");
        let req = a.build_request().unwrap();
        assert_eq!(req.online, cpus("0-11,20-21"));
        assert_eq!(req.offline, cpus("12-19"));
        assert_eq!(req.turbo, Some(false));
        assert_eq!(req.tune.len(), 2);
        for (t, id) in req.tune.iter().zip([20, 21]) {
            assert_eq!(t, &Tune { cpus: [id].into_iter().collect(), governor: Some("performance".into()), max_khz: Some(2_400_000), ..Default::default() });
        }
    }

    #[test]
    fn apply_with_nothing_staged_says_so() {
        let (f, live, _) = setup();
        let mut a = app(&live);
        press(&mut a, "w");
        assert_eq!(a.msg, "nothing staged");
        assert_eq!(f.read("cpu13/online"), "1");
    }

    #[test]
    fn apply_in_a_dry_run_writes_nothing_and_keeps_the_staging() {
        let (f, _, dry) = setup();
        let mut a = app(&dry);
        at(&mut a, 13);
        press(&mut a, " gw");
        assert!(a.msg.contains("dry run: 8 action(s) would be applied"), "{}", a.msg);
        assert!(a.dirty());
        assert_eq!((f.read("cpu13/online"), f.read("cpu12/cpufreq/scaling_governor")), ("1".to_string(), "powersave".to_string()));
    }

    #[test]
    fn apply_writes_the_staged_state_and_clears_it() {
        let (f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 13);
        press(&mut a, " g[tw");
        assert!(a.msg.starts_with("applied 16 change(s)"), "{}", a.msg);
        assert_eq!(f.read("cpu13/online"), "0");
        assert_eq!(f.read("intel_pstate/no_turbo"), "1");
        for id in [12, 14, 15, 16, 17, 18, 19] {
            assert_eq!(f.read(&format!("cpu{id}/cpufreq/scaling_governor")), "performance");
            assert_eq!(f.read(&format!("cpu{id}/cpufreq/scaling_max_freq")), "3700000");
        }
        assert_eq!(f.read("cpu13/cpufreq/scaling_governor"), "powersave", "the CPU going offline is not tuned");
        // The view now shows the new live state with nothing pending.
        assert!(!a.dirty());
        assert_eq!(a.desired, cpus("0-12,14-21"));
        assert_eq!(a.globals.no_turbo, Some(true));
    }

    #[test]
    fn a_failed_apply_keeps_what_was_staged() {
        let (f, live, _) = setup();
        let mut a = app(&live);
        at(&mut a, 13);
        press(&mut a, " ");
        at(&mut a, 20);
        press(&mut a, "g");
        // cpu13 cannot be switched (its knob is gone by the time we apply).
        std::fs::remove_file(f.root().join("cpu13/online")).unwrap();
        press(&mut a, "w");
        assert!(a.msg.contains("2 applied, 1 failed") && a.msg.contains("take cpu13 offline"), "{}", a.msg);
        assert_eq!(f.read("cpu20/cpufreq/scaling_governor"), "performance");
        // Nothing was thrown away: the user can fix the cause and press w again.
        assert!(a.dirty());
        assert!(!a.desired.contains(13));
        assert_eq!(a.staged.keys().copied().collect::<Vec<_>>(), [20, 21]);
    }

    #[test]
    fn refresh_picks_up_changes_made_elsewhere() {
        let (f, live, _) = setup();
        let mut a = app(&live);
        f.offline(5);
        f.write("intel_pstate/no_turbo", "1");
        press(&mut a, "r");
        assert_eq!(a.msg, "refreshed");
        assert!(!a.topo.get(5).unwrap().online);
        assert_eq!(a.globals.no_turbo, Some(true));
        // If the tree goes away the old view stays and the error is shown.
        std::fs::remove_file(f.root().join("present")).unwrap();
        a.refresh();
        assert!(a.msg.contains("refresh failed") && a.topo.cpus.len() == 22);
    }

    #[test]
    fn quit_keys() {
        let (_f, live, _) = setup();
        for (code, mods) in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut a = app(&live);
            a.on_key(code, mods);
            assert!(a.quit && !a.dirty(), "{code:?}");
        }
        let mut a = app(&live);
        at(&mut a, 3);
        press(&mut a, "cxyz9");
        assert!(!a.quit, "plain c toggles a core and unknown keys do nothing");
        assert_eq!(a.desired, cpus("0-2,5-21"));
    }

    #[test]
    fn the_screen_shows_live_and_staged_state() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        let s = screen(&mut a, 130, 40);
        for needle in ["cpu-control — live", "GOVERNOR", "LP-E", "fixed", "online", "powersave", "4.50 GHz", "target online 22/22", "P: 12/12", "turbo: on", "smt: on", "w apply"] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
        assert!(!s.contains("STAGED"));

        at(&mut a, 13);
        press(&mut a, " gt");
        let s = screen(&mut a, 130, 40);
        for needle in ["STAGED CHANGES (w to apply)", "-> off", "performance*", "target online 21/22", "E: 7/8", "turbo: off*", "turbo staged off"] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }

    #[test]
    fn the_screen_marks_offline_pending_and_unknown_cpus() {
        let f = fake_sys();
        f.offline(12);
        let ctx = Ctx::for_test(&f, false);
        let mut a = app(&ctx);
        let s = screen(&mut a, 130, 40);
        let row = s.lines().find(|l| l.contains("OFFLINE")).expect("an OFFLINE row");
        assert!(row.contains("12") && row.matches('?').count() == 2, "type and core should be `?`: {row}");
        press(&mut a, "a");
        assert!(screen(&mut a, 130, 40).contains("-> on"));
    }

    #[test]
    fn the_preset_popup_is_drawn_over_the_table() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        press(&mut a, "m");
        let s = screen(&mut a, 130, 40);
        for needle in ["presets (Enter stage, Esc cancel)", "no-pcores", "Disable all P-cores", "quiet"] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }

    #[test]
    fn drawing_survives_any_terminal_size() {
        let (_f, live, _) = setup();
        let mut a = app(&live);
        press(&mut a, "m");
        for (w, h) in [(1, 1), (5, 2), (20, 4), (40, 12), (300, 100)] {
            screen(&mut a, w, h);
        }
        a.popup = None;
        for (w, h) in [(1, 1), (10, 3), (79, 24)] {
            screen(&mut a, w, h);
        }
    }

    #[test]
    fn centering_never_leaves_the_area() {
        let area = Rect::new(3, 2, 40, 10);
        assert_eq!(centered(area, 20, 4), Rect::new(13, 5, 20, 4));
        assert_eq!(centered(area, 500, 500), area);
        assert_eq!(centered(Rect::new(0, 0, 0, 0), 70, 13), Rect::new(0, 0, 0, 0));
    }
}
