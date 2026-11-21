//! Native GUI (egui). Edits are *staged*; "Apply" writes them. When not root,
//! the write is delegated to `pkexec cpu-control preset <staged-profile>` so the
//! GUI itself never runs privileged and the privileged side is just the
//! validated CLI path.

use crate::cpuset::CpuSet;
use crate::ctx::{self, Ctx};
use crate::plan::fmt_khz;
use crate::profile::{self, Profile, ProfileFile, TuneEntry};
use crate::topology::{CoreKind, Cpu, Globals, Topology};
use anyhow::{Context, Result};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    tmp: PathBuf,
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
    let mut gui = Gui::new(ctx, topo, globals);
    if !gui.ctx.dry_run && !ctx::is_root() {
        gui.say("Not root: Apply will ask for authentication via pkexec");
    }
    eframe::run_native("CPU Control", opts, Box::new(|_| Ok(Box::new(gui)))).map_err(|e| anyhow::anyhow!("{e}"))
}

fn kind_color(k: CoreKind) -> Color32 {
    match k {
        CoreKind::P => Color32::from_rgb(180, 130, 255),
        CoreKind::E => Color32::from_rgb(100, 170, 255),
        CoreKind::LpE => Color32::from_rgb(90, 215, 200),
    }
}

impl Gui {
    fn new(ctx: Ctx, topo: Topology, globals: Globals) -> Gui {
        let mut log = Vec::new();
        if ctx.dry_run {
            log.push("DRY RUN: nothing will be written".to_string());
        }
        let file = ProfileFile::load(&ctx.profiles_path).unwrap_or_else(|e| {
            log.push(format!("saved profiles unavailable ({e:#}); showing built-in presets only"));
            ProfileFile::default()
        });
        Gui {
            desired: topo.online(),
            selected: topo.cpus.first().map_or(0, |c| c.id),
            topo,
            globals,
            turbo: None,
            staged: BTreeMap::new(),
            preset: None,
            presets: profile::available(file),
            scope: Scope::Core,
            log,
            last_refresh: Instant::now(),
            job: None,
            ctx,
        }
    }

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
            // The physical core; just this CPU while its topology is unknown.
            Scope::Core => sel.siblings.clone(),
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
                    let lo = c.min_khz.unwrap_or(0);
                    let hi = c.max_khz.unwrap_or(u64::MAX).max(lo);
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
            return self.apply_direct(&p);
        }
        // Unprivileged: hand the staged profile to pkexec.
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.say(format!("cannot locate own executable: {e}"));
                return;
            }
        };
        let tmp = match write_staged(p) {
            Ok(t) => t,
            Err(e) => {
                self.say(format!("could not write staged profile: {e:#}"));
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

    /// Apply in this process: we are root, or it is a dry run.
    fn apply_direct(&mut self, p: &Profile) {
        let res = profile::to_request(&self.topo, p).and_then(|req| self.ctx.execute(&self.topo, &req));
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
        self.frame(ui);
    }
}

/// Write the staged profile where the privileged helper can read it: a fresh
/// file only we can write, so what gets authenticated is what gets applied.
fn write_staged(p: Profile) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let mut file = ProfileFile::default();
    file.profile.insert(STAGED_NAME.into(), p);
    let text = toml::to_string_pretty(&file)?;
    let name = format!("cpu-control-{}-{}.toml", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let tmp = std::env::temp_dir().join(name);
    // `create_new` refuses an existing file or a symlink planted at this name.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    if let Err(e) = f.write_all(text.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    Ok(tmp)
}

impl Gui {
    /// One frame of the whole window.
    fn frame(&mut self, ui: &mut egui::Ui) {
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
            let (kind, core) = if cpu.topology_known {
                (cpu.kind.to_string(), cpu.core_id.to_string())
            } else {
                ("unknown".to_string(), "?".to_string())
            };
            ui.label(
                RichText::new(format!(
                    "{} core · core_id {} · L2 #{} · {}",
                    kind,
                    core,
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

        // Group threads into physical cores (keyed, and so ordered, by the
        // core's lowest CPU id). Sibling sets are used rather than core_id,
        // which is only a placeholder for a CPU whose topology is unknown.
        let mut cores: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for id in set.iter() {
            let c = self.topo.get(id).unwrap();
            cores.entry(c.siblings.iter().next().unwrap_or(id)).or_default().push(id);
        }

        ui.horizontal_wrapped(|ui| {
            for ids in cores.into_values() {
                let core_set: CpuSet = ids.iter().copied().collect();
                let first = self.topo.get(ids[0]).unwrap();
                let core_id = if first.topology_known { first.core_id.to_string() } else { "?".to_string() };
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

#[cfg(test)]
mod tests {
    // Nothing here may reach the pkexec branch of `Gui::apply`: tests only
    // call `apply` on a dry-run context and use `apply_direct` for real writes.
    use super::*;
    use crate::fake::{Fake, fake_sys};

    fn cpus(s: &str) -> CpuSet {
        CpuSet::parse_list(s).unwrap()
    }

    /// A fake machine whose topology is already cached, and a GUI over it.
    fn setup(dry_run: bool) -> (Fake, Gui) {
        let f = fake_sys();
        Ctx::for_test(&f, false).discover().unwrap();
        let g = gui(&f, dry_run);
        (f, g)
    }

    fn gui(f: &Fake, dry_run: bool) -> Gui {
        let ctx = Ctx::for_test(f, dry_run);
        let (topo, globals) = ctx.discover().unwrap();
        Gui::new(ctx, topo, globals)
    }

    fn toggle(g: &mut Gui, sel: &str) {
        g.handle(Act::ToggleGroup(cpus(sel), sel.to_string()));
    }

    fn logged(g: &Gui, needle: &str) -> bool {
        g.log.iter().any(|l| l.contains(needle))
    }

    /// Render one frame without a window.
    fn draw(g: &mut Gui) {
        let ctx = egui::Context::default();
        // No renderer here, so the font texture update is discarded on purpose.
        ctx.run_ui(egui::RawInput::default(), |ui| g.frame(ui)).drop_without_applying_deltas();
    }

    #[test]
    fn starts_clean_with_the_presets_loaded() {
        let (_f, g) = setup(false);
        assert!(!g.dirty() && g.log.is_empty() && g.job.is_none());
        assert_eq!((g.selected, g.desired.clone()), (0, cpus("0-21")));
        assert_eq!(g.presets.len(), 11);
        assert!(g.scope == Scope::Core);
        let (_f, dry) = setup(true);
        assert!(logged(&dry, "DRY RUN"));
    }

    #[test]
    fn a_broken_profiles_file_is_logged_and_builtins_remain() {
        let f = fake_sys();
        std::fs::write(f.dir.path().join("profiles.toml"), "[profile.x\n").unwrap();
        let g = gui(&f, false);
        assert_eq!(g.presets.len(), 11);
        assert!(logged(&g, "saved profiles unavailable"));

        std::fs::write(f.dir.path().join("profiles.toml"), "[profile.mine]\nonline = \"e\"\n").unwrap();
        let g = gui(&f, false);
        assert_eq!((g.presets.len(), g.presets[11].0.as_str()), (12, "mine"));
        assert!(g.log.is_empty());
    }

    #[test]
    fn toggling_stages_groups_on_and_off() {
        let (_f, mut g) = setup(false);
        toggle(&mut g, "13");
        assert_eq!(g.desired, cpus("0-12,14-21"));
        assert!(g.dirty());
        // A group with a member off comes fully on first, then goes off.
        toggle(&mut g, "12-15");
        assert_eq!(g.desired, cpus("0-21"));
        toggle(&mut g, "12-15");
        assert_eq!(g.desired, cpus("0-11,16-21"));
        // The fixed CPU is skipped inside a group and refused on its own.
        toggle(&mut g, "0,5");
        assert_eq!(g.desired, cpus("0-4,6-11,16-21"));
        toggle(&mut g, "0");
        assert!(g.desired.contains(0) && logged(&g, "0: cannot be taken offline"));
        // Ids that are not CPUs are ignored.
        toggle(&mut g, "90");
        assert!(g.desired.iter().all(|i| i < 22));
    }

    #[test]
    fn the_last_cpu_stays_staged_online() {
        let (_f, mut g) = setup(false);
        g.topo.cpus[0].hotpluggable = true;
        toggle(&mut g, "1-21");
        assert_eq!(g.desired, cpus("0"));
        toggle(&mut g, "0");
        assert_eq!(g.desired, cpus("0"));
        assert!(logged(&g, "At least one CPU must stay online"));
    }

    #[test]
    fn turbo_is_staged_only_when_it_differs_from_live() {
        let (_f, mut g) = setup(false);
        g.handle(Act::Turbo(false));
        assert_eq!(g.turbo, Some(false));
        assert!(g.dirty());
        g.handle(Act::Turbo(true));
        assert_eq!(g.turbo, None);
        assert!(!g.dirty());
    }

    #[test]
    fn tuning_scope_selects_cpu_core_kind_or_everything() {
        let (_f, mut g) = setup(false);
        g.handle(Act::Select(1));
        assert_eq!(g.selected, 1);
        for (scope, want) in [(Scope::Cpu, "1"), (Scope::Core, "1-2"), (Scope::Kind, "0-11"), (Scope::All, "0-21")] {
            assert_eq!(g.scope_set(scope), cpus(want));
        }
        // CPUs staged offline are never tuned.
        toggle(&mut g, "2,4-7");
        for (scope, want) in [(Scope::Core, "1"), (Scope::Kind, "0-1,3,8-11"), (Scope::All, "0-1,3,8-21")] {
            assert_eq!(g.scope_set(scope), cpus(want));
        }
        g.handle(Act::Select(2));
        assert!(g.scope_set(Scope::Cpu).is_empty());
        g.handle(Act::Select(99));
        assert!(g.scope_set(Scope::All).is_empty());
    }

    #[test]
    fn core_scope_of_an_unknown_cpu_is_that_cpu_alone() {
        // cpu12's placeholder core_id (12) equals the real core_id of cpu3/4.
        let f = fake_sys();
        f.offline(12);
        let mut g = gui(&f, false);
        toggle(&mut g, "12");
        g.handle(Act::Select(12));
        assert_eq!(g.scope_set(Scope::Core), cpus("12"));
        g.handle(Act::Select(3));
        assert_eq!(g.scope_set(Scope::Core), cpus("3-4"));
    }

    #[test]
    fn governor_and_epp_are_staged_for_the_scope() {
        let (_f, mut g) = setup(false);
        g.handle(Act::Select(13));
        g.scope = Scope::Cpu;
        g.handle(Act::SetGovernor("performance".into()));
        g.scope = Scope::Kind;
        g.handle(Act::SetEpp("power".into()));
        assert_eq!(g.staged.keys().copied().collect::<CpuSet>(), cpus("12-19"));
        assert_eq!(g.staged[&13].governor.as_deref(), Some("performance"));
        assert_eq!(g.staged[&12].governor, None);
        assert!(g.staged.values().all(|s| s.epp.as_deref() == Some("power")));

        // What the panel shows: staged values over live ones.
        let eff = g.effective(g.topo.get(13).unwrap());
        assert_eq!((eff.governor.as_deref(), eff.epp.as_deref()), (Some("performance"), Some("power")));
        assert_eq!((eff.min_khz, eff.max_khz), (Some(400_000), Some(3_800_000)));
        let eff = g.effective(g.topo.get(0).unwrap());
        assert_eq!((eff.governor.as_deref(), eff.epp.as_deref()), (Some("powersave"), Some("balance_performance")));
    }

    #[test]
    fn frequency_limits_are_clamped_per_cpu_and_stay_ordered() {
        let (_f, mut g) = setup(false);
        g.scope = Scope::All;
        g.handle(Act::SetFreq { min_khz: 3_000_000, max_khz: 4_600_000 });
        let lim = |g: &Gui, id: usize| (g.staged[&id].min_khz.unwrap(), g.staged[&id].max_khz.unwrap());
        assert_eq!(lim(&g, 1), (3_000_000, 4_600_000)); // 4.8 GHz core: as asked
        assert_eq!(lim(&g, 0), (3_000_000, 4_500_000)); // capped at its 4.5 GHz
        assert_eq!(lim(&g, 12), (3_000_000, 3_800_000));
        assert_eq!(lim(&g, 20), (2_500_000, 2_500_000)); // both ends pulled into range
        g.handle(Act::SetFreq { min_khz: 100, max_khz: 200 });
        assert!(g.staged.values().all(|s| s.min_khz == Some(400_000) && s.max_khz == Some(400_000)));
        // Inverted input cannot produce min > max.
        g.handle(Act::SetFreq { min_khz: 3_000_000, max_khz: 1_000_000 });
        assert!(g.staged.values().all(|s| s.min_khz <= s.max_khz));

        g.handle(Act::ResetFreq);
        assert_eq!(lim(&g, 1), (400_000, 4_800_000));
        assert_eq!(lim(&g, 20), (400_000, 2_500_000));
    }

    #[test]
    fn nonsense_hardware_limits_do_not_panic_the_clamp() {
        let (_f, mut g) = setup(false);
        g.topo.cpus[0].min_khz = Some(5_000_000); // above its own maximum
        g.scope = Scope::Cpu;
        g.handle(Act::SetFreq { min_khz: 1_000_000, max_khz: 2_000_000 });
        assert!(g.staged[&0].min_khz <= g.staged[&0].max_khz);
    }

    #[test]
    fn staging_a_preset_sets_the_target_and_clears_manual_tuning() {
        let (_f, mut g) = setup(false);
        g.scope = Scope::All;
        g.handle(Act::SetGovernor("performance".into()));
        let (name, quiet) = g.presets.iter().find(|(n, _)| n == "quiet").cloned().unwrap();
        g.handle(Act::Preset(name, quiet.clone()));
        assert_eq!(g.desired, cpus("0,12-21"), "cpu0 cannot go offline");
        assert_eq!(g.turbo, Some(false));
        assert!(g.staged.is_empty() && g.preset == Some(quiet) && g.dirty());
        assert!(logged(&g, "Preset `quiet` staged"));

        // A preset that does not fit this machine changes nothing.
        let before = g.desired.clone();
        g.handle(Act::Preset("bad".into(), Profile { online: Some("40-50".into()), ..Default::default() }));
        assert_eq!(g.desired, before);
        assert!(logged(&g, "Preset `bad`:"));

        // One without an online/offline rule keeps the staged online set.
        toggle(&mut g, "12");
        g.handle(Act::Preset("gov".into(), Profile { governor: Some("performance".into()), ..Default::default() }));
        assert_eq!(g.desired, cpus("0,13-21"));
        assert_eq!(g.turbo, None);
    }

    #[test]
    fn the_staged_profile_describes_exactly_what_is_on_screen() {
        let (_f, mut g) = setup(false);
        let (name, quiet) = g.presets.iter().find(|(n, _)| n == "quiet").cloned().unwrap();
        g.handle(Act::Preset(name, quiet));
        g.handle(Act::Turbo(true)); // live value: unstages the preset's turbo...
        g.handle(Act::Turbo(false)); // ...and this stages it again explicitly
        g.handle(Act::Select(12));
        g.scope = Scope::Kind;
        g.handle(Act::SetFreq { min_khz: 800_000, max_khz: 2_000_000 });
        g.scope = Scope::Cpu;
        g.handle(Act::SetGovernor("performance".into()));
        toggle(&mut g, "19"); // tuned above, now staged offline: must be dropped

        let p = g.build_profile();
        assert_eq!(p.online.as_deref(), Some("0,12-18,20-21"));
        assert_eq!((p.offline.clone(), p.turbo), (None, Some(false)));
        assert_eq!((p.governor.as_deref(), p.epp.as_deref()), (Some("powersave"), Some("power")), "the preset's defaults survive");
        assert_eq!(p.description.as_deref(), Some("Staged from GUI"));
        let mut tune: Vec<_> = p.tune.iter().map(|t| (t.cpus.as_str(), t.governor.as_deref(), t.min_freq.as_deref(), t.max_freq.as_deref())).collect();
        tune.sort();
        assert_eq!(tune, [("12", Some("performance"), Some("800000khz"), Some("2000000khz")), ("13-18", None, Some("800000khz"), Some("2000000khz"))]);

        // And it resolves to a request the planner accepts.
        let req = profile::to_request(&g.topo, &p).unwrap();
        assert_eq!((req.online.clone(), req.offline.clone()), (cpus("0,12-18,20-21"), cpus("1-11,19")));
        assert!(g.ctx.plan(&g.topo, &req).is_ok());
    }

    #[test]
    fn apply_with_nothing_staged_only_logs() {
        let (f, mut g) = setup(true);
        g.handle(Act::Apply);
        assert!(logged(&g, "Nothing staged") && g.job.is_none());
        assert_eq!(f.read("cpu13/online"), "1");
    }

    #[test]
    fn a_dry_run_apply_logs_the_plan_and_keeps_the_staging() {
        let (f, mut g) = setup(true);
        toggle(&mut g, "13");
        g.handle(Act::Turbo(false));
        g.handle(Act::Apply);
        assert!(logged(&g, "would: take cpu13 offline") && logged(&g, "would: turbo boost disabled"));
        assert!(logged(&g, "warning: online CPUs will be non-contiguous"));
        assert!(g.dirty() && g.job.is_none());
        assert_eq!((f.read("cpu13/online"), f.read("intel_pstate/no_turbo")), ("1".to_string(), "0".to_string()));
    }

    #[test]
    fn a_direct_apply_writes_refreshes_and_clears_the_staging() {
        let (f, mut g) = setup(false);
        toggle(&mut g, "20-21");
        g.handle(Act::Select(12));
        g.scope = Scope::Kind;
        g.handle(Act::SetGovernor("performance".into()));
        g.handle(Act::Turbo(false));
        let p = g.build_profile();
        g.apply_direct(&p);
        assert!(logged(&g, "did: take cpu20 offline") && logged(&g, "11 applied, 0 failed"), "{:?}", g.log);
        assert_eq!((f.read("cpu20/online"), f.read("cpu21/online")), ("0".to_string(), "0".to_string()));
        assert_eq!(f.read("cpu19/cpufreq/scaling_governor"), "performance");
        assert_eq!(f.read("intel_pstate/no_turbo"), "1");
        assert!(!g.dirty());
        assert_eq!(g.desired, cpus("0-19"));
        assert_eq!(g.globals.no_turbo, Some(true));
    }

    #[test]
    fn a_direct_apply_that_fails_keeps_the_staging() {
        let (f, mut g) = setup(false);
        toggle(&mut g, "13-14");
        std::fs::remove_file(f.root().join("cpu13/online")).unwrap();
        let p = g.build_profile();
        g.apply_direct(&p);
        assert!(logged(&g, "FAILED: take cpu13 offline") && logged(&g, "1 applied, 1 failed"), "{:?}", g.log);
        assert!(g.dirty() && !g.desired.contains(13));

        // An invalid staged profile is an error line, not a panic.
        g.apply_direct(&Profile { online: Some("none".into()), ..Default::default() });
        assert!(logged(&g, "error: "));
    }

    #[test]
    fn the_staged_profile_file_is_private_fresh_and_parseable() {
        use std::os::unix::fs::PermissionsExt;
        let p = Profile { online: Some("0-3".into()), governor: Some("powersave".into()), ..Default::default() };
        let a = write_staged(p.clone()).unwrap();
        let b = write_staged(p.clone()).unwrap();
        assert_ne!(a, b, "every apply gets its own file");
        assert!(a.starts_with(std::env::temp_dir()));
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        // The privileged side loads it like any profiles file.
        let file = ProfileFile::load(&a).unwrap();
        assert_eq!(profile::lookup(&file, STAGED_NAME), Some(p));
        for f in [a, b] {
            std::fs::remove_file(f).unwrap();
        }
    }

    /// A GUI with a finished helper job reporting `result`.
    fn finished(result: Result<(i32, String), String>) -> (Fake, Gui, PathBuf) {
        let (f, mut g) = setup(false);
        toggle(&mut g, "13");
        let tmp = f.dir.path().join("staged.toml");
        std::fs::write(&tmp, "").unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(result).unwrap();
        g.job = Some(Job { rx, tmp: tmp.clone() });
        (f, g, tmp)
    }

    #[test]
    fn a_pending_helper_job_blocks_a_second_apply() {
        let (f, mut g) = setup(false);
        toggle(&mut g, "13");
        let (_tx, rx) = mpsc::channel();
        g.job = Some(Job { rx, tmp: f.dir.path().join("staged.toml") });
        let lines = g.log.len();
        g.handle(Act::Apply); // must return before doing anything
        assert_eq!(g.log.len(), lines);
        // Still running: polling leaves it in place.
        g.poll_job(&egui::Context::default());
        assert!(g.job.is_some() && g.dirty());
    }

    #[test]
    fn helper_success_refreshes_and_clears_the_staging() {
        let (f, mut g, tmp) = finished(Ok((0, "  did: take cpu13 offline\n\n1 change(s) applied, 0 failed\n".into())));
        f.offline(13); // what the privileged helper did
        g.poll_job(&egui::Context::default());
        assert!(g.job.is_none() && !tmp.exists(), "the staged file is removed");
        assert!(logged(&g, "did: take cpu13 offline") && logged(&g, "Applied"));
        assert!(!g.log.iter().any(|l| l.trim().is_empty()));
        assert!(!g.dirty() && !g.topo.get(13).unwrap().online);
    }

    #[test]
    fn helper_failures_keep_the_staging_and_explain() {
        for (result, needle) in [
            (Ok((126, String::new())), "Authentication dismissed"),
            (Ok((127, String::new())), "not authorized"),
            (Ok((1, "failed: take cpu13 offline: boom\n".to_string())), "Finished with errors (exit 1)"),
            (Ok((-1, String::new())), "Finished with errors (exit -1)"),
            (Err("could not run pkexec: No such file or directory".to_string()), "could not run pkexec"),
        ] {
            let (_f, mut g, tmp) = finished(result);
            g.poll_job(&egui::Context::default());
            assert!(logged(&g, needle), "{needle}: {:?}", g.log);
            assert!(g.job.is_none() && !tmp.exists());
            assert!(g.dirty() && !g.desired.contains(13), "{needle}: staging must survive");
        }
    }

    #[test]
    fn discard_and_refresh() {
        let (f, mut g) = setup(false);
        toggle(&mut g, "13");
        g.handle(Act::Turbo(false));
        g.handle(Act::SetGovernor("performance".into()));
        g.handle(Act::Discard);
        assert!(!g.dirty() && g.staged.is_empty() && g.preset.is_none());

        f.offline(5);
        g.handle(Act::Refresh);
        assert!(!g.topo.get(5).unwrap().online);
        // The change came from outside, so it shows as a pending difference.
        assert!(g.dirty() && g.desired.contains(5));

        std::fs::remove_file(f.root().join("present")).unwrap();
        g.handle(Act::Refresh);
        assert!(logged(&g, "refresh failed") && g.topo.cpus.len() == 22);
    }

    #[test]
    fn the_log_is_bounded() {
        let (_f, mut g) = setup(false);
        for i in 0..500 {
            g.say(format!("line {i}"));
        }
        assert_eq!(g.log.len(), 200);
        assert_eq!((g.log[0].as_str(), g.log[199].as_str()), ("line 300", "line 499"));
    }

    #[test]
    fn every_panel_renders_headless_in_every_state() {
        let (f, mut g) = setup(true);
        draw(&mut g);

        // Staged hotplug, tuning and a preset; an offline CPU selected.
        toggle(&mut g, "13");
        g.handle(Act::Select(13));
        g.handle(Act::Turbo(false));
        draw(&mut g);
        g.handle(Act::Select(1));
        g.scope = Scope::Kind;
        g.handle(Act::SetGovernor("performance".into()));
        g.handle(Act::SetFreq { min_khz: 800_000, max_khz: 2_000_000 });
        let (name, p) = g.presets[9].clone();
        g.handle(Act::Preset(name, p));
        g.handle(Act::Apply); // dry run: fills the log
        draw(&mut g);

        // A helper job in flight (spinner, Apply disabled).
        let (_tx, rx) = mpsc::channel();
        g.job = Some(Job { rx, tmp: f.dir.path().join("none.toml") });
        draw(&mut g);
        g.job = None;

        // A selection that no longer exists, and missing global knobs.
        g.handle(Act::Select(99));
        g.globals = Globals::default();
        draw(&mut g);

        // CPUs really offline, one of them with unknown topology.
        let f = fake_sys();
        f.offline(12);
        f.offline(2);
        let mut g = gui(&f, true);
        for id in [12, 2, 0, 21] {
            g.handle(Act::Select(id));
            draw(&mut g);
        }
        // CPUs without cpufreq at all (no sliders, no bars).
        for c in &mut g.topo.cpus {
            (c.min_khz, c.max_khz, c.cur_khz, c.governor, c.epp) = (None, None, None, None, None);
        }
        draw(&mut g);
    }
}
