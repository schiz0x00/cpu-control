//! Turning a high-level [`Request`] into an ordered list of sysfs writes.

use crate::cpuset::CpuSet;
use crate::topology::{Cpu, Topology, cpu_dir};
use anyhow::{Context, Result, bail};
use std::fs;
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

#[derive(Clone, Debug)]
pub struct Action {
    pub path: PathBuf,
    pub value: String,
    pub desc: String,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub warnings: Vec<String>,
    /// Online set after the plan completes (excluding SMT global changes).
    pub final_online: CpuSet,
}

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
    if !(v > 0.0) {
        bail!("frequency must be positive: `{s}`");
    }
    Ok((v * mult).round() as u64)
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
        });
    }
    // 2. Take CPUs down.
    for id in offline.iter().filter(|&i| topo.get(i).unwrap().online) {
        plan.actions.push(Action {
            path: cpu_dir(root, id).join("online"),
            value: "0".into(),
            desc: format!("take cpu{id} offline"),
        });
    }

    // 3. Global knobs.
    if let Some(on) = req.smt {
        plan.actions.push(Action {
            path: root.join("smt/control"),
            value: if on { "on" } else { "off" }.into(),
            desc: format!("SMT {}", if on { "on" } else { "off" }),
        });
    }
    if let Some(turbo) = req.turbo {
        let path = root.join("intel_pstate/no_turbo");
        if path.exists() {
            plan.actions.push(Action {
                path,
                value: if turbo { "0" } else { "1" }.into(),
                desc: format!("turbo boost {}", if turbo { "enabled" } else { "disabled" }),
            });
        } else {
            let boost = root.join("cpufreq/boost");
            if boost.exists() {
                plan.actions.push(Action {
                    path: boost,
                    value: if turbo { "1" } else { "0" }.into(),
                    desc: format!("boost {}", if turbo { "enabled" } else { "disabled" }),
                });
            } else {
                plan.warnings.push("no turbo/boost control found on this system".into());
            }
        }
    }

    // 4. Per-CPU tuning (only CPUs that will be online).
    for t in req.tune.iter().filter(|t| !t.is_noop()) {
        for id in t.cpus.iter() {
            let Some(cpu) = topo.get(id) else { bail!("cpu{id} does not exist") };
            if !final_online.contains(id) {
                plan.warnings.push(format!("cpu{id} will be offline; skipping its frequency/governor settings"));
                continue;
            }
            tune_actions(&mut plan, root, cpu, t)?;
        }
    }

    plan.final_online = final_online;
    Ok(plan)
}

fn tune_actions(plan: &mut Plan, root: &Path, cpu: &Cpu, t: &Tune) -> Result<()> {
    let cf = cpu_dir(root, cpu.id).join("cpufreq");
    let id = cpu.id;
    if let Some(g) = &t.governor {
        plan.actions.push(Action {
            path: cf.join("scaling_governor"),
            value: g.clone(),
            desc: format!("cpu{id} governor -> {g}"),
        });
    }
    if let Some(e) = &t.epp {
        plan.actions.push(Action {
            path: cf.join("energy_performance_preference"),
            value: e.clone(),
            desc: format!("cpu{id} energy preference -> {e}"),
        });
    }
    let (cur_min, cur_max) = (cpu.scaling_min_khz, cpu.scaling_max_khz);
    if let (Some(lo), Some(hi)) = (t.min_khz, t.max_khz) {
        if lo > hi {
            bail!("min frequency ({}) is above max frequency ({})", fmt_khz(lo), fmt_khz(hi));
        }
    }
    let min = Action {
        path: cf.join("scaling_min_freq"),
        value: String::new(),
        desc: String::new(),
    };
    let mk_min = |v: u64| Action { value: v.to_string(), desc: format!("cpu{id} min freq -> {}", fmt_khz(v)), ..min.clone() };
    let mk_max = |v: u64| Action {
        path: cf.join("scaling_max_freq"),
        value: v.to_string(),
        desc: format!("cpu{id} max freq -> {}", fmt_khz(v)),
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
    Ok(())
}

pub struct ApplyReport {
    pub applied: usize,
    pub failed: Vec<(Action, std::io::Error)>,
}

/// Execute a plan. Failures don't abort: remaining actions still run.
pub fn apply(plan: &Plan) -> ApplyReport {
    let mut report = ApplyReport { applied: 0, failed: Vec::new() };
    for a in &plan.actions {
        match fs::write(&a.path, &a.value) {
            Ok(()) => report.applied += 1,
            Err(e) => report.failed.push((a.clone(), e)),
        }
    }
    report
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
}
