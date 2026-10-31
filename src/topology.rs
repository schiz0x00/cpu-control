//! Discovery of CPU topology and live state from sysfs.
//!
//! All paths are relative to a configurable sysfs root so the whole stack can
//! be tested against a fake tree.

use crate::cpuset::CpuSet;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Kind of core on a (possibly hybrid) CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CoreKind {
    /// Performance core (or any core on a non-hybrid CPU).
    P,
    /// Efficiency core.
    E,
    /// Low-power efficiency core (Meteor Lake+ SoC tile; no L3 access).
    LpE,
}

impl fmt::Display for CoreKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CoreKind::P => "P",
            CoreKind::E => "E",
            CoreKind::LpE => "LP-E",
        })
    }
}

#[derive(Clone, Debug)]
pub struct Cpu {
    pub id: usize,
    pub kind: CoreKind,
    /// Kernel `core_id` (shared by SMT siblings).
    pub core_id: usize,
    pub package: usize,
    /// All hardware threads on the same physical core, including self.
    pub siblings: CpuSet,
    /// CPUs sharing the L2 cache (a "cluster").
    pub l2_group: CpuSet,
    /// `false` if the kernel exposes no `online` file (e.g. the boot CPU).
    pub hotpluggable: bool,
    pub online: bool,
    /// `false` when this CPU is offline and no cached topology was available,
    /// so `siblings`/`l2_group`/`core_id` are guesses.
    pub topology_known: bool,
    pub min_khz: Option<u64>,
    pub max_khz: Option<u64>,
    pub cur_khz: Option<u64>,
    pub scaling_min_khz: Option<u64>,
    pub scaling_max_khz: Option<u64>,
    pub governor: Option<String>,
    pub epp: Option<String>,
}

impl Cpu {
    /// True if this thread is not the lowest-numbered sibling of its core.
    pub fn is_secondary_thread(&self) -> bool {
        self.siblings.iter().next().is_some_and(|first| first != self.id)
    }
}

#[derive(Clone, Debug)]
pub struct Topology {
    pub cpus: Vec<Cpu>,
}

/// Global (non per-CPU) knobs.
#[derive(Clone, Debug, Default)]
pub struct Globals {
    pub smt_control: Option<String>,
    pub no_turbo: Option<bool>,
    pub pstate_status: Option<String>,
    pub available_governors: Vec<String>,
    pub available_epp: Vec<String>,
}

pub fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

pub fn cpu_dir(root: &Path, id: usize) -> PathBuf {
    root.join(format!("cpu{id}"))
}

impl Topology {
    /// Discover topology under `root` (normally `/sys/devices/system/cpu`).
    /// `devices_root` is `/sys/devices`, used for hybrid core-type PMUs.
    pub fn discover(root: &Path, devices_root: &Path, cache: Option<&TopologyCache>) -> Result<Topology> {
        let possible = read_trim(root.join("present"))
            .or_else(|| read_trim(root.join("possible")))
            .with_context(|| format!("cannot read CPU list under {}", root.display()))?;
        let present = CpuSet::parse_list(&possible)?;

        let p_set = read_set(&devices_root.join("cpu_core/cpus"));
        let e_set = read_set(&devices_root.join("cpu_atom/cpus"));
        let lp_set = read_set(&devices_root.join("cpu_lowpower/cpus"));

        let mut cpus = Vec::new();
        for id in present.iter() {
            let dir = cpu_dir(root, id);
            if !dir.is_dir() {
                continue;
            }
            let hotpluggable = dir.join("online").exists();
            let online = if hotpluggable { read_trim(dir.join("online")).as_deref() == Some("1") } else { true };

            let topo = dir.join("topology");
            let core_id = read_u64(topo.join("core_id")).unwrap_or(id as u64) as usize;
            let package = read_u64(topo.join("physical_package_id")).unwrap_or(0) as usize;
            let siblings = read_trim(topo.join("thread_siblings_list"))
                .and_then(|s| CpuSet::parse_list(&s).ok())
                .unwrap_or_else(|| [id].into_iter().collect());

            let l2_group = find_cache_group(&dir, 2).unwrap_or_else(|| siblings.clone());
            let has_l3 = find_cache_group(&dir, 3).is_some();

            // Offline CPUs hide their topology/cache entries; `apply_cache`
            // repairs those from the persisted cache afterwards.
            let kind = if lp_set.contains(id) {
                CoreKind::LpE
            } else if e_set.contains(id) {
                // LP-E cores sit on the SoC tile with no L3 access.
                if online && !has_l3 && !p_set.is_empty() { CoreKind::LpE } else { CoreKind::E }
            } else {
                CoreKind::P
            };

            let cf = dir.join("cpufreq");
            cpus.push(Cpu {
                id,
                kind,
                core_id,
                package,
                siblings,
                l2_group,
                hotpluggable,
                online,
                topology_known: online,
                min_khz: read_u64(cf.join("cpuinfo_min_freq")),
                max_khz: read_u64(cf.join("cpuinfo_max_freq")),
                cur_khz: read_u64(cf.join("scaling_cur_freq")),
                scaling_min_khz: read_u64(cf.join("scaling_min_freq")),
                scaling_max_khz: read_u64(cf.join("scaling_max_freq")),
                governor: read_trim(cf.join("scaling_governor")),
                epp: read_trim(cf.join("energy_performance_preference")),
            });
        }
        let mut topo = Topology { cpus };
        topo.apply_cache(cache);
        Ok(topo)
    }

    pub fn get(&self, id: usize) -> Option<&Cpu> {
        self.cpus.iter().find(|c| c.id == id)
    }

    pub fn all(&self) -> CpuSet {
        self.cpus.iter().map(|c| c.id).collect()
    }

    pub fn online(&self) -> CpuSet {
        self.cpus.iter().filter(|c| c.online).map(|c| c.id).collect()
    }

    pub fn of_kind(&self, kind: CoreKind) -> CpuSet {
        self.cpus.iter().filter(|c| c.kind == kind).map(|c| c.id).collect()
    }

    /// Second (and later) hardware threads of each physical core.
    pub fn smt_secondaries(&self) -> CpuSet {
        self.cpus.iter().filter(|c| c.is_secondary_thread()).map(|c| c.id).collect()
    }
}

impl Globals {
    pub fn read(root: &Path) -> Globals {
        let cpu0 = cpu_dir(root, 0).join("cpufreq");
        let words = |p: PathBuf| -> Vec<String> {
            read_trim(p).map(|s| s.split_whitespace().map(String::from).collect()).unwrap_or_default()
        };
        Globals {
            smt_control: read_trim(root.join("smt/control")),
            no_turbo: read_trim(root.join("intel_pstate/no_turbo")).map(|s| s == "1"),
            pstate_status: read_trim(root.join("intel_pstate/status")),
            available_governors: words(cpu0.join("scaling_available_governors")),
            available_epp: words(cpu0.join("energy_performance_available_preferences")),
        }
    }
}

fn read_set(path: &Path) -> CpuSet {
    read_trim(path).and_then(|s| CpuSet::parse_list(&s).ok()).unwrap_or_default()
}

/// Find the shared-CPU list of the cache at the given level, if any.
fn find_cache_group(cpu_dir: &Path, level: u64) -> Option<CpuSet> {
    let rd = fs::read_dir(cpu_dir.join("cache")).ok()?;
    for entry in rd.flatten() {
        let p = entry.path();
        if read_u64(p.join("level")) == Some(level)
            && read_trim(p.join("type")).as_deref() != Some("Instruction")
        {
            return read_trim(p.join("shared_cpu_list")).and_then(|s| CpuSet::parse_list(&s).ok());
        }
    }
    None
}

/// Static per-CPU facts remembered while a CPU is online, because the kernel
/// removes `topology/` and `cache/` for offline CPUs.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedCpu {
    kind: CoreKind,
    core_id: usize,
    package: usize,
    siblings: Vec<usize>,
    l2_group: Vec<usize>,
    min_khz: Option<u64>,
    max_khz: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TopologyCache {
    /// The `present` CPU list this cache was recorded for (hardware fingerprint).
    present: String,
    cpu: BTreeMap<String, CachedCpu>,
}

impl TopologyCache {
    pub fn load(path: &Path) -> Option<TopologyCache> {
        toml::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    pub fn save(path: &Path, topo: &Topology) -> Result<()> {
        let present = topo.all().to_string();
        // Start from the old cache so facts about currently-offline CPUs survive.
        let mut cache = TopologyCache::load(path).filter(|c| c.present == present).unwrap_or_default();
        cache.present = present;
        for c in topo.cpus.iter().filter(|c| c.topology_known) {
            cache.cpu.insert(
                c.id.to_string(),
                CachedCpu {
                    kind: c.kind,
                    core_id: c.core_id,
                    package: c.package,
                    siblings: c.siblings.iter().collect(),
                    l2_group: c.l2_group.iter().collect(),
                    min_khz: c.min_khz,
                    max_khz: c.max_khz,
                },
            );
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, toml::to_string_pretty(&cache)?)?;
        Ok(())
    }
}

impl Topology {
    /// Fill in facts for offline CPUs from the cache (if it matches this hardware).
    fn apply_cache(&mut self, cache: Option<&TopologyCache>) {
        let Some(cache) = cache.filter(|c| c.present == self.all().to_string()) else {
            return;
        };
        for cpu in self.cpus.iter_mut().filter(|c| !c.online) {
            if let Some(c) = cache.cpu.get(&cpu.id.to_string()) {
                cpu.kind = c.kind;
                cpu.core_id = c.core_id;
                cpu.package = c.package;
                cpu.siblings = c.siblings.iter().copied().collect();
                cpu.l2_group = c.l2_group.iter().copied().collect();
                cpu.min_khz = cpu.min_khz.or(c.min_khz);
                cpu.max_khz = cpu.max_khz.or(c.max_khz);
                cpu.topology_known = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::fake_sys;

    /// Mimic the kernel: an offlined CPU loses its topology/ and cache/ dirs.
    fn offline_like_kernel(f: &crate::fake::Fake, id: usize) {
        let d = f.root().join(format!("cpu{id}"));
        fs::write(d.join("online"), "0\n").unwrap();
        fs::remove_dir_all(d.join("topology")).unwrap();
        fs::remove_dir_all(d.join("cache")).unwrap();
        fs::remove_dir_all(d.join("cpufreq")).unwrap();
    }

    #[test]
    fn cache_preserves_topology_of_offline_cpus() {
        let f = fake_sys();
        let cache_path = f.dir.path().join("state/topology.toml");
        let before = f.discover();
        TopologyCache::save(&cache_path, &before).unwrap();

        // Offline an SMT sibling (cpu2) and an LP-E core (cpu20).
        offline_like_kernel(&f, 2);
        offline_like_kernel(&f, 20);

        // Without the cache the facts are lost...
        let blind = Topology::discover(&f.root(), &f.devices(), None).unwrap();
        assert!(!blind.get(2).unwrap().topology_known);
        assert_eq!(blind.get(20).unwrap().kind, CoreKind::E);

        // ...with it, they are recovered.
        let cache = TopologyCache::load(&cache_path).unwrap();
        let topo = Topology::discover(&f.root(), &f.devices(), Some(&cache)).unwrap();
        let c2 = topo.get(2).unwrap();
        assert!(!c2.online && c2.topology_known && c2.is_secondary_thread());
        assert_eq!(c2.core_id, 8);
        assert_eq!(topo.get(20).unwrap().kind, CoreKind::LpE);
        assert_eq!(topo.smt_secondaries().to_string(), "2,4-5,7,9,11");
    }

    #[test]
    fn saving_keeps_facts_about_offline_cpus() {
        let f = fake_sys();
        let path = f.dir.path().join("topology.toml");
        TopologyCache::save(&path, &f.discover()).unwrap();
        offline_like_kernel(&f, 2);
        let cache = TopologyCache::load(&path).unwrap();
        let topo = Topology::discover(&f.root(), &f.devices(), Some(&cache)).unwrap();
        // Re-saving while cpu2 is offline must not clobber its cached entry.
        TopologyCache::save(&path, &topo).unwrap();
        let again = TopologyCache::load(&path).unwrap();
        assert_eq!(again.cpu["2"].core_id, 8);
    }

    #[test]
    fn stale_cache_for_other_hardware_is_ignored() {
        let f = fake_sys();
        let path = f.dir.path().join("topology.toml");
        TopologyCache::save(&path, &f.discover()).unwrap();
        let mut cache = TopologyCache::load(&path).unwrap();
        cache.present = "0-7".into();
        offline_like_kernel(&f, 2);
        let topo = Topology::discover(&f.root(), &f.devices(), Some(&cache)).unwrap();
        assert!(!topo.get(2).unwrap().topology_known);
    }
}
