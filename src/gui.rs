//! Native GUI (egui). Edits are *staged*; "Apply" writes them. When not root,
//! the write is delegated to `pkexec cpu-control preset <staged-profile>` so the
//! GUI itself never runs privileged and the privileged side is just the
//! validated CLI path.

use crate::cpuset::CpuSet;
use crate::ctx::{self, Ctx};
use crate::plan::fmt_khz;
use crate::profile::{self, Profile, ProfileFile, TuneEntry};
use crate::topology::{CoreKind, Cpu, Globals, Topology};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const STAGED_NAME: &str = "gui-staged";

#[derive(Clone, Default)]
struct Staged {
    governor: Option<String>,
    epp: Option<String>,
    min_khz: Option<u64>,
    max_khz: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Cpu,
    Core,
    Kind,
    All,
}

/// Deferred UI actions, so drawing code can borrow `self` immutably.
enum Act {
    Select(usize),
    ToggleGroup(CpuSet, String),
    Preset(String, Profile),
    Turbo(bool),
    SetGovernor(String),
    SetEpp(String),
    SetFreq { min_khz: u64, max_khz: u64 },
    ResetFreq,
    Apply,
    Discard,
    Refresh,
}

struct Job {
    rx: Receiver<Result<(i32, String), String>>,
    tmp: std::path::PathBuf,
}

struct Gui {
    ctx: Ctx,
    topo: Topology,
    globals: Globals,
    desired: CpuSet,
    turbo: Option<bool>,
    staged: BTreeMap<usize, Staged>,
    preset: Option<Profile>,
    presets: Vec<(String, Profile)>,
    selected: usize,
    scope: Scope,
    log: Vec<String>,
    last_refresh: Instant,
    job: Option<Job>,
}

pub fn run(ctx: Ctx, topo: Topology, globals: Globals) -> Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]).with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    let mut log = Vec::new();
    if ctx.dry_run {
        log.push("DRY RUN: nothing will be written".to_string());
    } else if !ctx::is_root() {
        log.push("Not root: Apply will ask for authentication via pkexec".to_string());
    }
    let gui = Gui {
        desired: topo.online(),
        selected: topo.cpus.first().map_or(0, |c| c.id),
        topo,
        globals,
        turbo: None,
        staged: BTreeMap::new(),
        preset: None,
        presets: load_presets(&ctx),
        scope: Scope::Core,
        log,
        last_refresh: Instant::now(),
        job: None,
        ctx,
    };
    eframe::run_native("CPU Control", opts, Box::new(|_| Ok(Box::new(gui)))).map_err(|e| anyhow::anyhow!("{e}"))
}

fn load_presets(ctx: &Ctx) -> Vec<(String, Profile)> {
    let file = ProfileFile::load(&ctx.profiles_path).unwrap_or_default();
    let mut v: Vec<_> = profile::builtin()
        .into_iter()
        .filter(|(n, _)| !file.profile.contains_key(*n))
        .map(|(n, p)| (n.to_string(), p))
        .collect();
    v.extend(file.profile);
    v
}

fn kind_color(k: CoreKind) -> Color32 {
    match k {
        CoreKind::P => Color32::from_rgb(180, 130, 255),
        CoreKind::E => Color32::from_rgb(100, 170, 255),
        CoreKind::LpE => Color32::from_rgb(90, 215, 200),
    }
}

impl Gui {
    fn dirty(&self) -> bool {
        self.desired != self.topo.online() || self.turbo.is_some() || !self.staged.is_empty() || self.preset.is_some()
    }

    fn say(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn refresh(&mut self) {
        match self.ctx.discover() {
            Ok((t, g)) => {
                self.topo = t;
                self.globals = g;
            }
            Err(e) => self.say(format!("refresh failed: {e}")),
        }
        self.last_refresh = Instant::now();
    }

    fn discard(&mut self) {
        self.desired = self.topo.online();
        self.turbo = None;
        self.staged.clear();
        self.preset = None;
    }

    fn scope_set(&self, scope: Scope) -> CpuSet {
        let Some(sel) = self.topo.get(self.selected) else { return CpuSet::new() };
        let set: CpuSet = match scope {
            Scope::Cpu => [sel.id].into_iter().collect(),
            Scope::Core => self.topo.cpus.iter().filter(|c| c.package == sel.package && c.core_id == sel.core_id).map(|c| c.id).collect(),
            Scope::Kind => self.topo.of_kind(sel.kind),
            Scope::All => self.topo.all(),
        };
        set.intersection(&self.desired) // can only tune CPUs that will be online
    }

    fn effective(&self, cpu: &Cpu) -> Staged {
        let s = self.staged.get(&cpu.id).cloned().unwrap_or_default();
        Staged {
            governor: s.governor.or_else(|| cpu.governor.clone()),
            epp: s.epp.or_else(|| cpu.epp.clone()),
            min_khz: s.min_khz.or(cpu.scaling_min_khz),
            max_khz: s.max_khz.or(cpu.scaling_max_khz),
        }
    }

    fn toggle_group(&mut self, group: CpuSet, what: &str) {
        let changeable: Vec<usize> = group.iter().filter(|&i| self.topo.get(i).is_some_and(|c| c.hotpluggable)).collect();
        if changeable.is_empty() {
            self.say(format!("{what}: cannot be taken offline (no hotplug support)"));
            return;
        }
        let all_on = changeable.iter().all(|&i| self.desired.contains(i));
        let mut next = self.desired.clone();
        for &i in &changeable {
            if all_on { next.remove(i) } else { next.insert(i) }
        }
        if next.is_empty() {
            self.say("At least one CPU must stay online");
            return;
        }
        self.desired = next;
    }

    fn handle(&mut self, act: Act) {
        match act {
            Act::Select(id) => self.selected = id,
            Act::ToggleGroup(set, what) => self.toggle_group(set, &what),
            Act::Preset(name, p) => match profile::to_request(&self.topo, &p) {
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
                    self.say(format!("Preset `{name}` staged - review, then Apply"));
                }
                Err(e) => self.say(format!("Preset `{name}`: {e}")),
            },
            Act::Turbo(on) => {
                let live = self.globals.no_turbo.map(|n| !n);
                self.turbo = if Some(on) == live { None } else { Some(on) };
            }
            Act::SetGovernor(g) => {
                for id in self.scope_set(self.scope).iter() {
                    self.staged.entry(id).or_default().governor = Some(g.clone());
                }
            }
            Act::SetEpp(e) => {
                for id in self.scope_set(self.scope).iter() {
                    self.staged.entry(id).or_default().epp = Some(e.clone());
                }
            }
            Act::SetFreq { min_khz, max_khz } => {
                for id in self.scope_set(self.scope).iter() {
                    let Some(c) = self.topo.get(id) else { continue };
                    let (lo, hi) = (c.min_khz.unwrap_or(0), c.max_khz.unwrap_or(u64::MAX));
                    let s = self.staged.entry(id).or_default();
                    // Always stage both ends so the pair stays consistent.
                    s.min_khz = Some(min_khz.clamp(lo, hi));
                    s.max_khz = Some(max_khz.clamp(lo, hi).max(min_khz.clamp(lo, hi)));
                }
            }
            Act::ResetFreq => {
                for id in self.scope_set(self.scope).iter() {
                    let Some(c) = self.topo.get(id) else { continue };
                    let s = self.staged.entry(id).or_default();
                    s.min_khz = c.min_khz;
                    s.max_khz = c.max_khz;
                }
            }
            Act::Apply => self.apply(),
            Act::Discard => self.discard(),
            Act::Refresh => self.refresh(),
        }
    }

    fn build_profile(&self) -> Profile {
        let mut p = self.preset.clone().unwrap_or_default();
        p.description = Some("Staged from GUI".into());
        p.online = Some(self.desired.to_string());
        p.offline = None;
        if self.turbo.is_some() {
            p.turbo = self.turbo;
        }
        type Key = (Option<String>, Option<String>, Option<u64>, Option<u64>);
        let mut groups: BTreeMap<Key, CpuSet> = BTreeMap::new();
        for (id, s) in &self.staged {
            if self.desired.contains(*id) {
                groups.entry((s.governor.clone(), s.epp.clone(), s.min_khz, s.max_khz)).or_default().insert(*id);
            }
        }
        for ((governor, epp, min, max), set) in groups {
            p.tune.push(TuneEntry {
                cpus: set.to_string(),
                governor,
                epp,
                min_freq: min.map(|k| format!("{k}khz")),
                max_freq: max.map(|k| format!("{k}khz")),
            });
        }
        p
    }

    fn apply(&mut self) {
        if self.job.is_some() {
            return;
        }
        if !self.dirty() {
            self.say("Nothing staged");
            return;
        }
        let p = self.build_profile();
        if self.ctx.dry_run || ctx::is_root() {
            let res = profile::to_request(&self.topo, &p).and_then(|req| self.ctx.execute(&self.topo, &req));
            match res {
                Ok(o) => {
                    for w in &o.warnings {
                        self.say(format!("warning: {w}"));
                    }
                    for a in &o.actions {
                        self.say(format!("{}: {a}", if o.dry_run { "would" } else { "did" }));
                    }
                    for f in &o.failures {
                        self.say(format!("FAILED: {f}"));
                    }
                    if !o.dry_run {
                        self.say(format!("{} applied, {} failed", o.applied, o.failures.len()));
                        self.refresh();
                        if o.failures.is_empty() {
                            self.discard();
                        }
                    }
                }
                Err(e) => self.say(format!("error: {e:#}")),
            }
            return;
        }
        // Unprivileged: hand the staged profile to pkexec.
        let mut file = ProfileFile::default();
        file.profile.insert(STAGED_NAME.into(), p);
        let tmp = std::env::temp_dir().join(format!("cpu-control-{}-{}.toml", std::process::id(), self.log.len()));
        let written = toml::to_string_pretty(&file).map_err(anyhow::Error::from).and_then(|s| {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(s.as_bytes())?;
            Ok(())
        });
        if let Err(e) = written {
            self.say(format!("could not write staged profile: {e}"));
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.say(format!("cannot locate own executable: {e}"));
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let tmp2 = tmp.clone();
        std::thread::spawn(move || {
            let out = std::process::Command::new("pkexec")
                .arg(exe)
                .arg("--profiles")
                .arg(&tmp2)
                .args(["preset", STAGED_NAME])
                .output();
            let _ = tx.send(match out {
                Ok(o) => Ok((
                    o.status.code().unwrap_or(-1),
                    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
                )),
                Err(e) => Err(format!("could not run pkexec: {e}")),
            });
        });
        self.say("Waiting for authentication...");
        self.job = Some(Job { rx, tmp });
    }

    fn poll_job(&mut self, egui_ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        let Ok(result) = job.rx.try_recv() else {
            egui_ctx.request_repaint_after(Duration::from_millis(200));
            return;
        };
        let job = self.job.take().unwrap();
        let _ = std::fs::remove_file(&job.tmp);
        match result {
            Ok((code, text)) => {
                for l in text.lines().filter(|l| !l.trim().is_empty()) {
                    self.say(l.to_string());
                }
                match code {
                    0 => {
                        self.say("Applied");
                        self.refresh();
                        self.discard();
                    }
                    126 => self.say("Authentication dismissed - nothing changed"),
                    127 => self.say("Authentication failed or not authorized - nothing changed"),
                    c => {
                        self.say(format!("Finished with errors (exit {c})"));
                        self.refresh();
                    }
                }
            }
            Err(e) => self.say(e),
        }
    }
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let egui_ctx = ui.ctx().clone();
        self.poll_job(&egui_ctx);
        if self.job.is_none() && self.last_refresh.elapsed() > Duration::from_secs(1) {
            self.refresh();
        }
        egui_ctx.request_repaint_after(Duration::from_millis(1000));

        let mut acts: Vec<Act> = Vec::new();
        self.top_bar(ui, &mut acts);
        self.log_panel(ui);
        self.tuning_panel(ui, &mut acts);
        self.core_grid(ui, &mut acts);
        for a in acts {
            self.handle(a);
        }
    }
}

impl Gui {
    fn top_bar(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.heading("CPU Control");
                ui.separator();
                ui.menu_button("Presets ▾", |ui| {
                    for (name, p) in &self.presets {
                        let desc = p.description.clone().unwrap_or_else(|| profile::describe(p));
                        if ui.button(name).on_hover_text(desc).clicked() {
                            acts.push(Act::Preset(name.clone(), p.clone()));
                            ui.close();
                        }
                    }
                });
                ui.separator();
                match self.globals.no_turbo.map(|n| !n) {
                    Some(live) => {
                        let mut on = self.turbo.unwrap_or(live);
                        if ui.checkbox(&mut on, if self.turbo.is_some() { "Turbo *" } else { "Turbo" }).changed() {
                            acts.push(Act::Turbo(on));
                        }
                    }
                    None => {
                        ui.label("Turbo: n/a");
                    }
                }
                ui.label(format!("SMT: {}", self.globals.smt_control.as_deref().unwrap_or("n/a")));
                ui.separator();
                let busy = self.job.is_some();
                let dirty = self.dirty();
                if ui.add_enabled(dirty && !busy, egui::Button::new(RichText::new("Apply").strong()).fill(Color32::from_rgb(40, 110, 60))).clicked() {
                    acts.push(Act::Apply);
                }
                if ui.add_enabled(dirty && !busy, egui::Button::new("Discard")).clicked() {
                    acts.push(Act::Discard);
                }
                if ui.button("Refresh").clicked() {
                    acts.push(Act::Refresh);
                }
                if busy {
                    ui.spinner();
                }
                ui.separator();
                let mut parts = Vec::new();
                for k in [CoreKind::P, CoreKind::E, CoreKind::LpE] {
                    let all = self.topo.of_kind(k);
                    if !all.is_empty() {
                        parts.push(format!("{k} {}/{}", all.intersection(&self.desired).len(), all.len()));
                    }
                }
                let text = format!("Target online: {}/{}  ({})", self.desired.len(), self.topo.cpus.len(), parts.join("  "));
                ui.label(if dirty { RichText::new(text).color(Color32::YELLOW) } else { RichText::new(text) });
            });
            ui.add_space(4.0);
        });
    }

    fn log_panel(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log").resizable(true).default_size(90.0).show(ui, |ui| {
            ui.label(RichText::new("Log").small().weak());
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for l in &self.log {
                    ui.label(RichText::new(l).monospace().small());
                }
            });
        });
    }

    fn tuning_panel(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        egui::Panel::right("tuning").default_size(290.0).show(ui, |ui| {
            ui.add_space(6.0);
            let Some(cpu) = self.topo.get(self.selected) else { return };
            ui.heading(format!("cpu{}", cpu.id));
            ui.label(
                RichText::new(format!(
                    "{} core · core_id {} · L2 #{} · {}",
                    cpu.kind,
                    cpu.core_id,
                    cpu.l2_group.iter().next().unwrap_or(0),
                    if cpu.online { "online" } else { "offline" }
                ))
                .color(kind_color(cpu.kind)),
            );
            if !cpu.hotpluggable {
                ui.label(RichText::new("Cannot be taken offline (no hotplug support)").small().weak());
            }
            ui.separator();
            let targets = self.scope_set(self.scope);
            ui.label(RichText::new(format!("Tuning applies to {} CPU(s): {}", targets.len(), targets)).small().weak());
            ui.label(RichText::new("(change the scope above the core grid)").small().weak());
            ui.separator();

            if !self.desired.contains(cpu.id) {
                ui.label("This CPU is staged offline: nothing to tune.");
                return;
            }
            let eff = self.effective(cpu);

            ui.label("Governor");
            let mut gov = eff.governor.clone().unwrap_or_default();
            egui::ComboBox::from_id_salt("gov").selected_text(&gov).show_ui(ui, |ui| {
                for g in &self.globals.available_governors {
                    if ui.selectable_value(&mut gov, g.clone(), g).clicked() {
                        acts.push(Act::SetGovernor(g.clone()));
                    }
                }
            });

            if !self.globals.available_epp.is_empty() {
                ui.add_space(6.0);
                ui.label("Energy/performance preference");
                let mut epp = eff.epp.clone().unwrap_or_default();
                egui::ComboBox::from_id_salt("epp").selected_text(&epp).show_ui(ui, |ui| {
                    for e in &self.globals.available_epp {
                        if ui.selectable_value(&mut epp, e.clone(), e).clicked() {
                            acts.push(Act::SetEpp(e.clone()));
                        }
                    }
                });
            }

            if let (Some(lo), Some(hi)) = (cpu.min_khz, cpu.max_khz) {
                ui.add_space(6.0);
                let (mut min_mhz, mut max_mhz) =
                    (eff.min_khz.unwrap_or(lo) as f64 / 1000.0, eff.max_khz.unwrap_or(hi) as f64 / 1000.0);
                let range = (lo as f64 / 1000.0)..=(hi as f64 / 1000.0);
                ui.label("Max frequency");
                let max_changed = ui.add(egui::Slider::new(&mut max_mhz, range.clone()).suffix(" MHz").step_by(100.0)).changed();
                ui.label("Min frequency");
                let min_changed = ui.add(egui::Slider::new(&mut min_mhz, range).suffix(" MHz").step_by(100.0)).changed();
                if max_changed || min_changed {
                    // Dragging one end past the other pushes the other along.
                    if max_changed && max_mhz < min_mhz {
                        min_mhz = max_mhz;
                    }
                    if min_changed && min_mhz > max_mhz {
                        max_mhz = min_mhz;
                    }
                    acts.push(Act::SetFreq { min_khz: (min_mhz * 1000.0) as u64, max_khz: (max_mhz * 1000.0) as u64 });
                }
                if ui.button("Reset limits to hardware range").clicked() {
                    acts.push(Act::ResetFreq);
                }
            }
            ui.separator();
            ui.label(RichText::new("Live").small().weak());
            ui.label(format!("current {}", cpu.cur_khz.map(fmt_khz).unwrap_or_else(|| "-".into())));
        });
    }

    fn core_grid(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        egui::CentralPanel::default().show(ui, |ui| {
            // Scope selector lives here so it can mutate `self.scope` directly.
            ui.horizontal(|ui| {
                ui.label("Tune scope:");
                ui.selectable_value(&mut self.scope, Scope::Cpu, "this CPU");
                ui.selectable_value(&mut self.scope, Scope::Core, "its core");
                ui.selectable_value(&mut self.scope, Scope::Kind, "all of its type");
                ui.selectable_value(&mut self.scope, Scope::All, "everything");
                ui.separator();
                ui.label(RichText::new("click a thread to toggle it · ⏻ toggles a whole core").small().weak());
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for kind in [CoreKind::P, CoreKind::E, CoreKind::LpE] {
                    let set = self.topo.of_kind(kind);
                    if set.is_empty() {
                        continue;
                    }
                    self.kind_section(ui, kind, &set, acts);
                    ui.add_space(10.0);
                }
            });
        });
    }

    fn kind_section(&self, ui: &mut egui::Ui, kind: CoreKind, set: &CpuSet, acts: &mut Vec<Act>) {
        let color = kind_color(kind);
        let name = match kind {
            CoreKind::P => "Performance cores",
            CoreKind::E => "Efficiency cores",
            CoreKind::LpE => "Low-power efficiency cores",
        };
        let on = set.intersection(&self.desired).len();
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).heading().color(color));
            ui.label(RichText::new(format!("{on}/{} on", set.len())).weak());
            if ui.button(if on == set.len() { "Disable all" } else { "Enable all" }).clicked() {
                acts.push(Act::ToggleGroup(set.clone(), name.to_string()));
            }
            let smt = self.topo.smt_secondaries().intersection(set);
            if !smt.is_empty() && ui.button("Toggle SMT threads").on_hover_text("second hardware thread of each core").clicked() {
                acts.push(Act::ToggleGroup(smt, "SMT threads".into()));
            }
        });

        // Group threads into physical cores, ordered by lowest CPU id.
        let mut cores: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
        for id in set.iter() {
            let c = self.topo.get(id).unwrap();
            cores.entry((c.package, c.core_id)).or_default().push(id);
        }
        let mut cores: Vec<Vec<usize>> = cores.into_values().collect();
        cores.sort_by_key(|v| v[0]);

        ui.horizontal_wrapped(|ui| {
            for ids in cores {
                let core_set: CpuSet = ids.iter().copied().collect();
                let core_id = self.topo.get(ids[0]).unwrap().core_id;
                egui::Frame::group(ui.style()).stroke(Stroke::new(1.0, color.gamma_multiply(0.5))).show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("core {core_id}")).small().weak());
                            if ui.small_button("⏻").on_hover_text("toggle this whole core").clicked() {
                                acts.push(Act::ToggleGroup(core_set.clone(), format!("core {core_id}")));
                            }
                        });
                        ui.horizontal(|ui| {
                            for id in &ids {
                                self.cpu_button(ui, *id, acts);
                            }
                        });
                    });
                });
            }
        });
    }

    fn cpu_button(&self, ui: &mut egui::Ui, id: usize, acts: &mut Vec<Act>) {
        let c = self.topo.get(id).unwrap();
        let want = self.desired.contains(id);
        let pending = want != c.online;
        let fill = if !c.hotpluggable {
            Color32::from_rgb(85, 85, 60)
        } else if want {
            Color32::from_rgb(38, 110, 62)
        } else {
            Color32::from_rgb(55, 55, 60)
        };
        let stroke = if pending {
            Stroke::new(2.5, Color32::YELLOW)
        } else if id == self.selected {
            Stroke::new(2.0, Color32::WHITE)
        } else {
            Stroke::NONE
        };
        let eff = self.effective(c);
        let freq = if c.online { c.cur_khz.map(fmt_khz).unwrap_or_default() } else { "off".into() };
        ui.vertical(|ui| {
            let label = RichText::new(format!("{id}")).strong().size(16.0).color(if want { Color32::WHITE } else { Color32::GRAY });
            let resp = ui.add(egui::Button::new(label).fill(fill).stroke(stroke).min_size(Vec2::new(66.0, 34.0)));
            if resp.clicked() {
                acts.push(Act::Select(id));
                acts.push(Act::ToggleGroup([id].into_iter().collect(), format!("cpu{id}")));
            }
            if resp.secondary_clicked() {
                acts.push(Act::Select(id));
            }
            resp.on_hover_text(format!(
                "cpu{id}: {} · {}\ngovernor {} · limits {}–{}\nleft-click toggles, right-click selects",
                c.kind,
                if c.online { "online" } else { "offline" },
                eff.governor.as_deref().unwrap_or("-"),
                eff.min_khz.map(fmt_khz).unwrap_or_else(|| "-".into()),
                eff.max_khz.map(fmt_khz).unwrap_or_else(|| "-".into()),
            ));
            let frac = match (c.online, c.cur_khz, c.max_khz) {
                (true, Some(cur), Some(max)) if max > 0 => (cur as f32 / max as f32).clamp(0.0, 1.0),
                _ => 0.0,
            };
            ui.add(egui::ProgressBar::new(frac).desired_width(66.0).desired_height(4.0));
            ui.label(RichText::new(freq).small().weak());
        });
    }
}
