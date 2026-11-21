//! Turning a high-level [`Request`] into an ordered list of sysfs writes.

use crate::cpuset::CpuSet;
use crate::topology::{Cpu, Topology, cpu_dir};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Per-CPU frequency/governor settings applied to a set of CPUs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tune {
    pub cpus: CpuSet,
    pub governor: Option<String>,
    pub epp: Option<String>,
    pub min_khz: Option<u64>,
    pub max_khz: Option<u64>,
}

impl Tune {
    pub fn is_noop(&self) -> bool {
        self.governor.is_none() && self.epp.is_none() && self.min_khz.is_none() && self.max_khz.is_none()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub online: CpuSet,
    pub offline: CpuSet,
    pub tune: Vec<Tune>,
    pub turbo: Option<bool>,
    pub smt: Option<bool>,
}

/// What an action does to the online set; `apply` uses it to stop taking CPUs
/// down once bringing one up has failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Online,
    Offline,
    Other,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub path: PathBuf,
    pub value: String,
    pub desc: String,
    pub step: Step,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub warnings: Vec<String>,
    /// Online set after the plan completes (excluding SMT global changes).
    pub final_online: CpuSet,
}

/// Upper bound for a frequency limit (100 GHz): generous, but it keeps `inf`
/// and stray exponents from turning into absurd values written to sysfs.
const MAX_FREQ_KHZ: u64 = 100_000_000;

/// Parse a frequency: bare numbers are MHz; `khz`, `mhz`, `ghz` suffixes accepted.
pub fn parse_freq_khz(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = t.strip_suffix("ghz") {
        (n, 1_000_000.0)
    } else if let Some(n) = t.strip_suffix("mhz") {
        (n, 1_000.0)
    } else if let Some(n) = t.strip_suffix("khz") {
        (n, 1.0)
    } else {
        (t.as_str(), 1_000.0)
    };
    let v: f64 = num.trim().parse().with_context(|| format!("invalid frequency `{s}`"))?;
    if !v.is_finite() || v <= 0.0 {
        bail!("frequency must be positive: `{s}`");
    }
    let khz = (v * mult).round();
    if khz < 1.0 || khz > MAX_FREQ_KHZ as f64 {
        bail!("frequency `{s}` is out of range (1 kHz to {})", fmt_khz(MAX_FREQ_KHZ));
    }
    Ok(khz as u64)
}

pub fn fmt_khz(khz: u64) -> String {
    if khz >= 1_000_000 {
        format!("{:.2} GHz", khz as f64 / 1e6)
    } else {
        format!("{} MHz", khz / 1000)
    }
}

pub fn build_plan(topo: &Topology, root: &Path, req: &Request) -> Result<Plan> {
    let mut plan = Plan::default();
    let all = topo.all();

    for c in req.online.union(&req.offline).iter() {
        if !all.contains(c) {
            bail!("cpu{c} does not exist");
        }
    }
    if let Some(c) = req.online.intersection(&req.offline).iter().next() {
        bail!("cpu{c} is requested both online and offline");
    }

    // Drop offline requests that cannot be honoured.
    let mut offline = CpuSet::new();
    for id in req.offline.iter() {
        let cpu = topo.get(id).unwrap();
        if !cpu.hotpluggable {
            if cpu.online {
                plan.warnings.push(format!(
                    "cpu{id} cannot be taken offline (kernel exposes no hotplug control); leaving it online"
                ));
            }
            continue;
        }
        offline.insert(id);
    }

    let mut final_online = topo.online().union(&req.online).difference(&offline);
    // Non-hotpluggable CPUs are always online.
    for c in topo.cpus.iter().filter(|c| !c.hotpluggable) {
        final_online.insert(c.id);
    }
    if final_online.is_empty() {
        bail!("refusing to leave the system with zero online CPUs");
    }

    let unknown: CpuSet = topo.cpus.iter().filter(|c| !c.topology_known).map(|c| c.id).collect();
    if !unknown.is_empty() && !(req.online.is_empty() && req.offline.is_empty()) {
        plan.warnings.push(format!(
            "the type and core layout of offline CPU(s) {unknown} are not known yet, so selectors like p, e, smt and core:N only guess at them; bring every CPU online once to record it"
        ));
    }

    // Some monitors (seen: btop 1.4.6) crash when online CPU ids have gaps.
    if final_online.iter().enumerate().any(|(i, id)| i != id) && final_online != topo.online() {
        plan.warnings.push(format!(
            "online CPUs will be non-contiguous ({final_online}); some monitoring tools (e.g. btop 1.4.x) crash in that state"
        ));
    }

    // 1. Bring CPUs up first so we never dip toward zero online CPUs.
    for id in req.online.iter().filter(|&i| !topo.get(i).unwrap().online) {
        let cpu = topo.get(id).unwrap();
        if !cpu.hotpluggable {
            continue;
        }
        plan.actions.push(Action {
            path: cpu_dir(root, id).join("online"),
            value: "1".into(),
            desc: format!("bring cpu{id} online"),
            step: Step::Online,
        });
    }
    // 2. Take CPUs down.
    for id in offline.iter().filter(|&i| topo.get(i).unwrap().online) {
        plan.actions.push(Action {
            path: cpu_dir(root, id).join("online"),
            value: "0".into(),
            desc: format!("take cpu{id} offline"),
            step: Step::Offline,
        });
    }

    // 3. Global knobs.
    if let Some(on) = req.smt {
        plan.actions.push(Action {
            path: root.join("smt/control"),
            value: if on { "on" } else { "off" }.into(),
            desc: format!("SMT {}", if on { "on" } else { "off" }),
            step: Step::Other,
        });
    }
    if let Some(turbo) = req.turbo {
        let path = root.join("intel_pstate/no_turbo");
        if path.exists() {
            plan.actions.push(Action {
                path,
                value: if turbo { "0" } else { "1" }.into(),
                desc: format!("turbo boost {}", if turbo { "enabled" } else { "disabled" }),
                step: Step::Other,
            });
        } else {
            let boost = root.join("cpufreq/boost");
            if boost.exists() {
                plan.actions.push(Action {
                    path: boost,
                    value: if turbo { "1" } else { "0" }.into(),
                    desc: format!("boost {}", if turbo { "enabled" } else { "disabled" }),
                    step: Step::Other,
                });
            } else {
                plan.warnings.push("no turbo/boost control found on this system".into());
            }
        }
    }

    // 4. Per-CPU tuning (only CPUs that will be online).
    // Limits each CPU will have once the writes planned so far have run, so a
    // later entry is checked against an earlier one rather than stale live values.
    let mut limits: BTreeMap<usize, Limits> = BTreeMap::new();
    for t in req.tune.iter().filter(|t| !t.is_noop()) {
        if let Some(g) = &t.governor {
            check_token("governor", g)?;
        }
        if let Some(e) = &t.epp {
            check_token("energy preference", e)?;
        }
        for id in t.cpus.iter() {
            let Some(cpu) = topo.get(id) else { bail!("cpu{id} does not exist") };
            if !final_online.contains(id) {
                plan.warnings.push(format!("cpu{id} will be offline; skipping its frequency/governor settings"));
                continue;
            }
            let lim = limits.entry(id).or_insert((cpu.scaling_min_khz, cpu.scaling_max_khz));
            tune_actions(&mut plan, root, cpu, t, lim)?;
        }
    }

    plan.final_online = final_online;
    Ok(plan)
}

/// (min, max) scaling limits in kHz; `None` when unknown (e.g. the CPU is offline).
type Limits = (Option<u64>, Option<u64>);

/// Governor and EPP names are plain identifiers (EPP may also be a raw number).
/// Anything else cannot be valid, so reject it before it is written as root.
fn check_token(what: &str, value: &str) -> Result<()> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        bail!("invalid {what} `{}`: expected a name such as `powersave`", value.escape_debug());
    }
    Ok(())
}

fn tune_actions(plan: &mut Plan, root: &Path, cpu: &Cpu, t: &Tune, limits: &mut Limits) -> Result<()> {
    let cf = cpu_dir(root, cpu.id).join("cpufreq");
    let id = cpu.id;
    if let Some(g) = &t.governor {
        plan.actions.push(Action {
            path: cf.join("scaling_governor"),
            value: g.clone(),
            desc: format!("cpu{id} governor -> {g}"),
            step: Step::Other,
        });
    }
    if let Some(e) = &t.epp {
        plan.actions.push(Action {
            path: cf.join("energy_performance_preference"),
            value: e.clone(),
            desc: format!("cpu{id} energy preference -> {e}"),
            step: Step::Other,
        });
    }
    let (cur_min, cur_max) = *limits;
    if let (Some(lo), Some(hi)) = (t.min_khz, t.max_khz)
        && lo > hi
    {
        bail!("min frequency ({}) is above max frequency ({})", fmt_khz(lo), fmt_khz(hi));
    }
    let mk_min = |v: u64| Action {
        path: cf.join("scaling_min_freq"),
        value: v.to_string(),
        desc: format!("cpu{id} min freq -> {}", fmt_khz(v)),
        step: Step::Other,
    };
    let mk_max = |v: u64| Action {
        path: cf.join("scaling_max_freq"),
        value: v.to_string(),
        desc: format!("cpu{id} max freq -> {}", fmt_khz(v)),
        step: Step::Other,
    };
    // Order writes so the kernel never sees min > max.
    match (t.min_khz, t.max_khz) {
        (Some(lo), Some(hi)) => {
            if cur_max.is_some_and(|cm| lo > cm) {
                plan.actions.extend([mk_max(hi), mk_min(lo)]);
            } else {
                plan.actions.extend([mk_min(lo), mk_max(hi)]);
            }
        }
        (Some(lo), None) => {
            if let Some(cm) = cur_max.filter(|&cm| lo > cm) {
                bail!("cpu{id}: min {} exceeds current max {}; raise --max too", fmt_khz(lo), fmt_khz(cm));
            }
            plan.actions.push(mk_min(lo));
        }
        (None, Some(hi)) => {
            if let Some(cm) = cur_min.filter(|&cm| hi < cm) {
                bail!("cpu{id}: max {} is below current min {}; lower --min too", fmt_khz(hi), fmt_khz(cm));
            }
            plan.actions.push(mk_max(hi));
        }
        (None, None) => {}
    }
    *limits = (t.min_khz.or(cur_min), t.max_khz.or(cur_max));
    Ok(())
}

pub struct ApplyReport {
    pub applied: usize,
    pub failed: Vec<(Action, io::Error)>,
}

/// Execute a plan. Failures don't abort, remaining actions still run, with one
/// exception: once a CPU has failed to come online nothing is taken offline
/// any more, because the plan counted on that CPU to carry the load.
pub fn apply(plan: &Plan) -> ApplyReport {
    let mut report = ApplyReport { applied: 0, failed: Vec::new() };
    let mut online_failed = false;
    for a in &plan.actions {
        if online_failed && a.step == Step::Offline {
            report.failed.push((a.clone(), io::Error::other("skipped because a CPU could not be brought online")));
            continue;
        }
        match write_knob(&a.path, &a.value) {
            Ok(()) => report.applied += 1,
            Err(e) => {
                online_failed |= a.step == Step::Online;
                report.failed.push((a.clone(), e));
            }
        }
    }
    report
}

/// Write a value to an existing sysfs attribute. Never creates the file: a
/// knob that is missing is an error, not something to conjure up as root.
fn write_knob(path: &Path, value: &str) -> io::Result<()> {
    OpenOptions::new().write(true).truncate(true).open(path)?.write_all(value.as_bytes())
}

pub fn permission_hint(report: &ApplyReport) -> Option<&'static str> {
    report
        .failed
        .iter()
        .any(|(_, e)| e.kind() == std::io::ErrorKind::PermissionDenied)
        .then_some("permission denied: run with sudo (writing to sysfs requires root)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::fake_meteor_lake;
    use crate::selector::eval;

    #[test]
    fn freq_parsing() {
        assert_eq!(parse_freq_khz("2400").unwrap(), 2_400_000);
        assert_eq!(parse_freq_khz("2.4GHz").unwrap(), 2_400_000);
        assert_eq!(parse_freq_khz("800mhz").unwrap(), 800_000);
        assert_eq!(parse_freq_khz("1500000khz").unwrap(), 1_500_000);
        assert!(parse_freq_khz("fast").is_err());
        assert!(parse_freq_khz("0").is_err());
    }

    #[test]
    fn offline_skips_cpu0_with_warning() {
        let (f, topo) = fake_meteor_lake();
        let req = Request { offline: eval(&topo, "p").unwrap(), ..Default::default() };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        assert_eq!(plan.actions.len(), 11); // cpu1..=11
        assert!(plan.warnings.iter().any(|w| w.contains("cpu0")));
        assert!(plan.final_online.contains(0));
    }

    #[test]
    fn warns_on_non_contiguous_online_set() {
        let (f, topo) = fake_meteor_lake();
        let gap = Request { offline: eval(&topo, "13").unwrap(), ..Default::default() };
        assert!(build_plan(&topo, &f.root(), &gap).unwrap().warnings.iter().any(|w| w.contains("non-contiguous")));
        let prefix = Request { offline: eval(&topo, "20-21").unwrap(), ..Default::default() };
        assert!(build_plan(&topo, &f.root(), &prefix).unwrap().warnings.is_empty());
    }

    #[test]
    fn refuses_zero_online() {
        let (f, topo) = fake_meteor_lake();
        // Pretend cpu0 is hot-pluggable so nothing keeps the system alive.
        let mut topo = topo;
        topo.cpus[0].hotpluggable = true;
        let req = Request { offline: topo.all(), ..Default::default() };
        assert!(build_plan(&topo, &f.root(), &req).is_err());
    }

    #[test]
    fn online_before_offline_and_apply_writes() {
        let (f, mut topo) = fake_meteor_lake();
        // Pre-offline cpu12 on disk so the plan must bring it up.
        std::fs::write(f.root().join("cpu12/online"), "0\n").unwrap();
        topo.cpus[12].online = false;
        let req = Request {
            online: eval(&topo, "12").unwrap(),
            offline: eval(&topo, "13").unwrap(),
            ..Default::default()
        };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        assert_eq!(plan.actions[0].value, "1");
        assert_eq!(plan.actions[1].value, "0");
        let rep = apply(&plan);
        assert!(rep.failed.is_empty());
        assert_eq!(f.read("cpu12/online"), "1");
        assert_eq!(f.read("cpu13/online"), "0");
    }

    #[test]
    fn freq_write_ordering() {
        let (f, topo) = fake_meteor_lake();
        // Raising min above current max must write max first.
        let mut topo = topo;
        topo.cpus[12].scaling_max_khz = Some(1_000_000);
        let req = Request {
            tune: vec![Tune { cpus: [12].into_iter().collect(), min_khz: Some(2_000_000), max_khz: Some(3_000_000), ..Default::default() }],
            ..Default::default()
        };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        assert!(plan.actions[0].path.ends_with("scaling_max_freq"));
        assert!(plan.actions[1].path.ends_with("scaling_min_freq"));
        // And min > max is rejected outright.
        let bad = Request {
            tune: vec![Tune { cpus: [12].into_iter().collect(), min_khz: Some(3_000_000), max_khz: Some(2_000_000), ..Default::default() }],
            ..Default::default()
        };
        assert!(build_plan(&topo, &f.root(), &bad).is_err());
    }

    #[test]
    fn tuning_offline_cpu_is_skipped() {
        let (f, topo) = fake_meteor_lake();
        let req = Request {
            offline: [13].into_iter().collect(),
            tune: vec![Tune { cpus: [13].into_iter().collect(), governor: Some("performance".into()), ..Default::default() }],
            ..Default::default()
        };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert!(plan.warnings.iter().any(|w| w.contains("skipping its frequency")));
    }

    fn cpus(s: &str) -> CpuSet {
        CpuSet::parse_list(s).unwrap()
    }

    fn tune(sel: &str) -> Tune {
        Tune { cpus: cpus(sel), ..Default::default() }
    }

    /// `(file name, value)` of every action, in order.
    fn writes(plan: &Plan) -> Vec<(String, String)> {
        plan.actions.iter().map(|a| (a.path.file_name().unwrap().to_string_lossy().into_owned(), a.value.clone())).collect()
    }

    #[test]
    fn freq_parsing_accepts_units_spacing_and_case() {
        for (input, khz) in [
            ("800", 800_000),
            (" 800 ", 800_000),
            ("2.4 GHz", 2_400_000),
            ("2.4ghz", 2_400_000),
            ("1e3", 1_000_000),
            ("3500MHZ", 3_500_000),
            ("400000kHz", 400_000),
            ("0.5khz", 1),
            ("100ghz", 100_000_000),
        ] {
            assert_eq!(parse_freq_khz(input).unwrap(), khz, "{input:?}");
        }
    }

    #[test]
    fn freq_parsing_rejects_values_no_cpu_could_mean() {
        // `inf` and friends parse as f64 and used to reach sysfs as u64::MAX.
        for bad in ["inf", "infinity", "nan", "-inf", "-800", "0", "0.0", "1e30", "100.1ghz", "0.0001", "0.4khz", "", "ghz", "fast", "8 0 0"] {
            assert!(parse_freq_khz(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn freq_formatting() {
        for (khz, text) in [(0, "0 MHz"), (400_000, "400 MHz"), (999_999, "999 MHz"), (1_000_000, "1.00 GHz"), (4_800_000, "4.80 GHz")] {
            assert_eq!(fmt_khz(khz), text);
        }
    }

    #[test]
    fn a_tune_is_a_noop_only_when_it_sets_nothing() {
        assert!(tune("0-3").is_noop());
        assert!(!Tune { governor: Some("x".into()), ..tune("0") }.is_noop());
        assert!(!Tune { epp: Some("x".into()), ..tune("0") }.is_noop());
        assert!(!Tune { min_khz: Some(1), ..tune("0") }.is_noop());
        assert!(!Tune { max_khz: Some(1), ..tune("0") }.is_noop());
    }

    #[test]
    fn requests_that_change_nothing_plan_nothing() {
        let (f, topo) = fake_meteor_lake();
        for req in [
            Request::default(),
            Request { online: cpus("0-21"), ..Default::default() }, // already online
            Request { tune: vec![tune("0-21")], ..Default::default() }, // no-op tune
        ] {
            let plan = build_plan(&topo, &f.root(), &req).unwrap();
            assert!(plan.actions.is_empty() && plan.warnings.is_empty());
            assert_eq!(plan.final_online, cpus("0-21"));
        }
        f.offline(13);
        let topo = f.discover();
        let again = Request { offline: cpus("13"), ..Default::default() };
        assert!(build_plan(&topo, &f.root(), &again).unwrap().actions.is_empty());
    }

    #[test]
    fn rejects_cpus_that_do_not_exist_and_contradictions() {
        let (f, topo) = fake_meteor_lake();
        for req in [
            Request { online: cpus("22"), ..Default::default() },
            Request { offline: cpus("3,99"), ..Default::default() },
            Request { tune: vec![Tune { governor: Some("powersave".into()), ..tune("40") }], ..Default::default() },
        ] {
            let err = build_plan(&topo, &f.root(), &req).unwrap_err().to_string();
            assert!(err.contains("does not exist"), "{err}");
        }
        let both = Request { online: cpus("3-4"), offline: cpus("4-5"), ..Default::default() };
        assert!(build_plan(&topo, &f.root(), &both).unwrap_err().to_string().contains("cpu4 is requested both"));
    }

    #[test]
    fn a_fixed_cpu_is_never_planned_offline_and_always_counts_as_online() {
        let (f, topo) = fake_meteor_lake();
        let plan = build_plan(&topo, &f.root(), &Request { offline: cpus("0"), ..Default::default() }).unwrap();
        assert!(plan.actions.is_empty());
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.final_online, cpus("0-21"));
        // Everything else may go: cpu0 alone keeps the machine alive.
        let rest = build_plan(&topo, &f.root(), &Request { offline: cpus("0-21"), ..Default::default() }).unwrap();
        assert_eq!((rest.actions.len(), &rest.final_online), (21, &cpus("0")));
    }

    #[test]
    fn steps_run_in_the_order_up_down_global_tune() {
        let (f, _) = fake_meteor_lake();
        f.offline(12);
        let topo = f.discover();
        let req = Request {
            online: cpus("12"),
            offline: cpus("13"),
            smt: Some(false),
            turbo: Some(false),
            tune: vec![Tune { governor: Some("performance".into()), epp: Some("performance".into()), ..tune("14") }],
        };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        assert_eq!(
            writes(&plan),
            [("online", "1"), ("online", "0"), ("control", "off"), ("no_turbo", "1"), ("scaling_governor", "performance"), ("energy_performance_preference", "performance")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        assert_eq!(plan.actions.iter().map(|a| a.step).collect::<Vec<_>>(), [Step::Online, Step::Offline, Step::Other, Step::Other, Step::Other, Step::Other]);
        assert!(plan.actions[0].path.ends_with("cpu12/online") && plan.actions[1].path.ends_with("cpu13/online"));
        assert!(plan.actions[4].path.ends_with("cpu14/cpufreq/scaling_governor"));
        assert_eq!(plan.final_online, cpus("0-12,14-21"));
    }

    #[test]
    fn turbo_uses_intel_pstate_then_boost_then_warns() {
        let (f, topo) = fake_meteor_lake();
        let turbo = |on| Request { turbo: Some(on), ..Default::default() };
        // intel_pstate: the file is "no_turbo", so the value is inverted.
        assert_eq!(writes(&build_plan(&topo, &f.root(), &turbo(true)).unwrap()), [("no_turbo".to_string(), "0".to_string())]);
        assert_eq!(writes(&build_plan(&topo, &f.root(), &turbo(false)).unwrap()), [("no_turbo".to_string(), "1".to_string())]);

        std::fs::remove_dir_all(f.root().join("intel_pstate")).unwrap();
        let none = build_plan(&topo, &f.root(), &turbo(true)).unwrap();
        assert!(none.actions.is_empty() && none.warnings[0].contains("no turbo/boost control"));

        f.write("cpufreq/boost", "1");
        assert_eq!(writes(&build_plan(&topo, &f.root(), &turbo(true)).unwrap()), [("boost".to_string(), "1".to_string())]);
        assert_eq!(writes(&build_plan(&topo, &f.root(), &turbo(false)).unwrap()), [("boost".to_string(), "0".to_string())]);
    }

    #[test]
    fn smt_switch_writes_on_or_off() {
        let (f, topo) = fake_meteor_lake();
        for (on, value) in [(true, "on"), (false, "off")] {
            let plan = build_plan(&topo, &f.root(), &Request { smt: Some(on), ..Default::default() }).unwrap();
            assert_eq!(plan.actions.len(), 1);
            assert!(plan.actions[0].path.ends_with("smt/control"));
            assert_eq!(plan.actions[0].value, value);
        }
    }

    #[test]
    fn governor_and_epp_must_be_plain_names() {
        let (f, topo) = fake_meteor_lake();
        let plan_with = |gov: Option<&str>, epp: Option<&str>| {
            let t = Tune { governor: gov.map(String::from), epp: epp.map(String::from), ..tune("3") };
            build_plan(&topo, &f.root(), &Request { tune: vec![t], ..Default::default() })
        };
        for ok in ["powersave", "schedutil", "balance_power", "128", "amd-pstate"] {
            assert!(plan_with(Some(ok), None).is_ok() && plan_with(None, Some(ok)).is_ok(), "{ok}");
        }
        // These are written verbatim into a root-owned file: nothing but an
        // identifier may get through.
        for bad in ["", " ", "power save", "x\ny", "x\nALL ALL=(ALL) NOPASSWD: ALL", "../../etc", "a/b", "p\0", "gov;rm"] {
            assert!(plan_with(Some(bad), None).is_err(), "governor {bad:?}");
            assert!(plan_with(None, Some(bad)).is_err(), "epp {bad:?}");
        }
        // Even when every targeted CPU would be skipped as offline.
        let t = Tune { governor: Some("x y".into()), ..tune("13") };
        assert!(build_plan(&topo, &f.root(), &Request { offline: cpus("13"), tune: vec![t], ..Default::default() }).is_err());
    }

    #[test]
    fn a_lone_limit_is_checked_against_the_other_live_limit() {
        let (f, mut topo) = fake_meteor_lake();
        topo.cpus[12].scaling_min_khz = Some(1_000_000);
        topo.cpus[12].scaling_max_khz = Some(2_000_000);
        let one = |min, max| Request { tune: vec![Tune { min_khz: min, max_khz: max, ..tune("12") }], ..Default::default() };

        let err = build_plan(&topo, &f.root(), &one(Some(3_000_000), None)).unwrap_err().to_string();
        assert!(err.contains("exceeds current max"), "{err}");
        let err = build_plan(&topo, &f.root(), &one(None, Some(500_000))).unwrap_err().to_string();
        assert!(err.contains("below current min"), "{err}");

        // Equal to the other limit is fine.
        assert_eq!(writes(&build_plan(&topo, &f.root(), &one(Some(2_000_000), None)).unwrap()), [("scaling_min_freq".to_string(), "2000000".to_string())]);
        assert_eq!(writes(&build_plan(&topo, &f.root(), &one(None, Some(1_000_000))).unwrap()), [("scaling_max_freq".to_string(), "1000000".to_string())]);
    }

    #[test]
    fn a_pair_of_limits_is_written_in_the_safe_order() {
        let (f, mut topo) = fake_meteor_lake();
        topo.cpus[12].scaling_min_khz = Some(1_000_000);
        topo.cpus[12].scaling_max_khz = Some(2_000_000);
        let pair = |min, max| {
            let req = Request { tune: vec![Tune { min_khz: Some(min), max_khz: Some(max), ..tune("12") }], ..Default::default() };
            writes(&build_plan(&topo, &f.root(), &req).unwrap()).into_iter().map(|(file, _)| file).collect::<Vec<_>>()
        };
        // Moving the window up: raise the ceiling before the floor.
        assert_eq!(pair(2_500_000, 3_000_000), ["scaling_max_freq", "scaling_min_freq"]);
        // Moving it down (or within): floor first.
        assert_eq!(pair(400_000, 800_000), ["scaling_min_freq", "scaling_max_freq"]);
        assert_eq!(pair(1_500_000, 1_500_000), ["scaling_min_freq", "scaling_max_freq"]);
    }

    #[test]
    fn later_tune_entries_see_what_earlier_ones_set() {
        // A profile's defaults followed by an override: the override must be
        // judged against the defaults, not against the live values they replace.
        let (f, mut topo) = fake_meteor_lake();
        topo.cpus[12].scaling_min_khz = Some(400_000);
        topo.cpus[12].scaling_max_khz = Some(1_000_000);
        let raise_max = Tune { max_khz: Some(3_800_000), ..tune("12") };
        let raise_min = Tune { min_khz: Some(2_000_000), ..tune("12") };

        let alone = Request { tune: vec![raise_min.clone()], ..Default::default() };
        assert!(build_plan(&topo, &f.root(), &alone).is_err(), "2 GHz floor under a live 1 GHz ceiling");

        let together = Request { tune: vec![raise_max, raise_min], ..Default::default() };
        let plan = build_plan(&topo, &f.root(), &together).unwrap();
        assert_eq!(writes(&plan), [("scaling_max_freq".to_string(), "3800000".to_string()), ("scaling_min_freq".to_string(), "2000000".to_string())]);

        // The other direction: a lowered floor makes room for a lower ceiling,
        // and a raised floor rules one out.
        topo.cpus[12].scaling_min_khz = Some(2_000_000);
        topo.cpus[12].scaling_max_khz = Some(3_800_000);
        let lower_min = Tune { min_khz: Some(400_000), ..tune("12") };
        let lower_max = Tune { max_khz: Some(1_000_000), ..tune("12") };
        assert!(build_plan(&topo, &f.root(), &Request { tune: vec![lower_max.clone()], ..Default::default() }).is_err());
        assert!(build_plan(&topo, &f.root(), &Request { tune: vec![lower_min, lower_max], ..Default::default() }).is_ok());
        let raise_floor = Tune { min_khz: Some(3_000_000), ..tune("12") };
        let then_cap = Tune { max_khz: Some(2_500_000), ..tune("12") };
        assert!(build_plan(&topo, &f.root(), &Request { tune: vec![raise_floor, then_cap], ..Default::default() }).is_err());
    }

    #[test]
    fn a_cpu_coming_online_is_tuned_without_guessing_its_limits() {
        let (f, _) = fake_meteor_lake();
        f.offline(12);
        let topo = f.discover();
        let req = Request { online: cpus("12"), tune: vec![Tune { min_khz: Some(3_000_000), ..tune("12") }], ..Default::default() };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        assert_eq!(writes(&plan), [("online".to_string(), "1".to_string()), ("scaling_min_freq".to_string(), "3000000".to_string())]);
    }

    #[test]
    fn unknown_topology_is_called_out_when_hotplugging() {
        let (f, _) = fake_meteor_lake();
        f.offline(12);
        let topo = f.discover(); // no cache: cpu12 is guesswork
        let hotplug = Request { offline: cpus("14"), ..Default::default() };
        let warned = build_plan(&topo, &f.root(), &hotplug).unwrap().warnings;
        assert!(warned.iter().any(|w| w.contains("offline CPU(s) 12") && w.contains("not known yet")), "{warned:?}");
        // Tuning alone does not involve the guessed facts.
        let tuning = Request { tune: vec![Tune { governor: Some("performance".into()), ..tune("3") }], ..Default::default() };
        assert!(build_plan(&topo, &f.root(), &tuning).unwrap().warnings.is_empty());
    }

    #[test]
    fn an_existing_gap_is_not_reported_again() {
        let (f, _) = fake_meteor_lake();
        f.offline(13);
        let topo = f.discover();
        let tuning = Request { tune: vec![Tune { governor: Some("performance".into()), ..tune("3") }], ..Default::default() };
        let w = build_plan(&topo, &f.root(), &tuning).unwrap().warnings;
        assert!(!w.iter().any(|w| w.contains("non-contiguous")), "{w:?}");
    }

    #[test]
    fn apply_writes_exact_values_and_counts() {
        let (f, topo) = fake_meteor_lake();
        let req = Request {
            offline: cpus("20-21"),
            turbo: Some(false),
            tune: vec![Tune { governor: Some("performance".into()), max_khz: Some(2_000_000), ..tune("12-13") }],
            ..Default::default()
        };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        let report = apply(&plan);
        assert_eq!((report.applied, report.failed.len()), (plan.actions.len(), 0));
        assert_eq!(report.applied, 7);
        assert!(permission_hint(&report).is_none());
        // Raw contents: the old (longer) value must be gone, no stray newline.
        assert_eq!(std::fs::read_to_string(f.root().join("cpu20/online")).unwrap(), "0");
        assert_eq!(std::fs::read_to_string(f.root().join("cpu12/cpufreq/scaling_governor")).unwrap(), "performance");
        assert_eq!(f.read("cpu13/cpufreq/scaling_max_freq"), "2000000");
        assert_eq!(f.read("intel_pstate/no_turbo"), "1");
        assert_eq!(f.read("cpu14/cpufreq/scaling_governor"), "powersave"); // untouched
    }

    #[test]
    fn apply_never_creates_a_missing_knob() {
        let (f, topo) = fake_meteor_lake();
        let req = Request { tune: vec![Tune { governor: Some("performance".into()), ..tune("12-13") }], ..Default::default() };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        // The knob disappears between planning and applying (CPU unplugged,
        // driver without the attribute, or a wrong --sysfs-root).
        std::fs::remove_file(f.root().join("cpu12/cpufreq/scaling_governor")).unwrap();
        let report = apply(&plan);
        assert_eq!((report.applied, report.failed.len()), (1, 1));
        assert_eq!(report.failed[0].1.kind(), io::ErrorKind::NotFound);
        assert!(!f.exists("cpu12/cpufreq/scaling_governor"), "apply must not create files");
        assert_eq!(f.read("cpu13/cpufreq/scaling_governor"), "performance"); // the rest still ran
    }

    #[test]
    fn nothing_goes_offline_after_a_cpu_fails_to_come_online() {
        let (f, _) = fake_meteor_lake();
        f.offline(20);
        f.offline(21);
        let topo = f.discover();
        // `only 20-21`: up with the LP-E cores, down with everything else.
        let req = Request {
            online: cpus("20-21"),
            offline: cpus("0-19"),
            tune: vec![Tune { governor: Some("performance".into()), ..tune("21") }],
            ..Default::default()
        };
        let plan = build_plan(&topo, &f.root(), &req).unwrap();
        // cpu20 refuses to come up.
        std::fs::remove_file(f.root().join("cpu20/online")).unwrap();
        let report = apply(&plan);

        // cpu21 came up, cpu20 failed, and all 19 offline steps were skipped:
        // the machine is not left running on one LP-E core.
        assert_eq!(f.read("cpu21/online"), "1");
        for id in 1..20 {
            assert_eq!(f.read(&format!("cpu{id}/online")), "1", "cpu{id} was taken offline");
        }
        assert_eq!(report.failed.len(), 1 + 19 + 1); // cpu20, the skipped 19, cpu21's governor (no cpufreq dir in the fake until settle)
        let skipped = report.failed.iter().filter(|(a, e)| a.step == Step::Offline && e.to_string().contains("skipped")).count();
        assert_eq!(skipped, 19);
        assert_eq!(report.applied, 1);
    }

    #[test]
    fn a_failed_offline_does_not_stop_the_rest() {
        let (f, topo) = fake_meteor_lake();
        let plan = build_plan(&topo, &f.root(), &Request { offline: cpus("12-14"), ..Default::default() }).unwrap();
        std::fs::remove_file(f.root().join("cpu13/online")).unwrap();
        let report = apply(&plan);
        assert_eq!((report.applied, report.failed.len()), (2, 1));
        assert_eq!((f.read("cpu12/online"), f.read("cpu14/online")), ("0".to_string(), "0".to_string()));
    }

    #[test]
    fn permission_errors_produce_the_sudo_hint() {
        use std::os::unix::fs::PermissionsExt;
        if crate::ctx::is_root() {
            return; // root ignores file modes; nothing to observe
        }
        let (f, topo) = fake_meteor_lake();
        let knob = f.root().join("cpu13/online");
        std::fs::set_permissions(&knob, std::fs::Permissions::from_mode(0o444)).unwrap();
        let plan = build_plan(&topo, &f.root(), &Request { offline: cpus("13-14"), ..Default::default() }).unwrap();
        let report = apply(&plan);
        assert_eq!((report.applied, report.failed.len()), (1, 1));
        assert!(report.failed[0].0.desc.contains("cpu13"));
        assert!(permission_hint(&report).unwrap().contains("sudo"));
        assert_eq!(f.read("cpu13/online"), "1");
    }
}
