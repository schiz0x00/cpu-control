//! Discovery of CPU topology and live state from sysfs.
//!
//! All paths are relative to a configurable sysfs root so the whole stack can
//! be tested against a fake tree.

use crate::cpuset::CpuSet;
use crate::fsutil::write_atomic;
use anyhow::{Context, Result, bail};
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
    /// so `kind`/`siblings`/`l2_group`/`core_id` are guesses.
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
    /// `Some(true)` when turbo/boost is switched off; `None` without a control.
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
            // repairs those from the persisted cache afterwards. They also
            // vanish from the hybrid PMU lists, so without a cache an offline
            // CPU falls through to `P` here: a guess (`topology_known` is false).
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
        if cpus.is_empty() {
            bail!("no CPUs found under {}", root.display());
        }
        let mut topo = Topology { cpus };
        topo.apply_cache(cache);
        topo.merge_groups();
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
    pub fn read(root: &Path, topo: &Topology) -> Globals {
        // Any online CPU can answer; cpu0 is not guaranteed to be one everywhere.
        let words = |name: &str| -> Vec<String> {
            topo.cpus
                .iter()
                .filter(|c| c.online)
                .find_map(|c| read_trim(cpu_dir(root, c.id).join("cpufreq").join(name)))
                .map(|s| s.split_whitespace().map(String::from).collect())
                .unwrap_or_default()
        };
        Globals {
            smt_control: read_trim(root.join("smt/control")),
            // Same precedence as the plan: intel_pstate's switch, else the
            // generic cpufreq boost flag (which has the opposite polarity).
            no_turbo: read_trim(root.join("intel_pstate/no_turbo"))
                .map(|s| s == "1")
                .or_else(|| read_trim(root.join("cpufreq/boost")).map(|s| s == "0")),
            pstate_status: read_trim(root.join("intel_pstate/status")),
            available_governors: words("scaling_available_governors"),
            available_epp: words("energy_performance_available_preferences"),
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
        let text = toml::to_string_pretty(&cache)?;
        // The UIs re-discover every second: leave an up-to-date file alone.
        if fs::read_to_string(path).is_ok_and(|old| old == text) {
            return Ok(());
        }
        write_atomic(path, &text)?;
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

    /// The kernel drops an offline CPU from its relatives' `thread_siblings_list`
    /// and cache `shared_cpu_list`, so the live lists shrink as CPUs go down
    /// while the cached list of the offline CPU still names everyone. Merge all
    /// overlapping lists so every group is whole and symmetric whatever happens
    /// to be online (and so the shrunken lists never reach the cache).
    fn merge_groups(&mut self) {
        fn find(leader: &BTreeMap<usize, usize>, mut id: usize) -> usize {
            while leader[&id] != id {
                id = leader[&id];
            }
            id
        }
        let fields: [fn(&mut Cpu) -> &mut CpuSet; 2] = [|c| &mut c.siblings, |c| &mut c.l2_group];
        let all = self.all();
        for field in fields {
            // Union-find: each id chains to the lowest id of its group.
            let mut leader: BTreeMap<usize, usize> = all.iter().map(|id| (id, id)).collect();
            for cpu in self.cpus.iter_mut() {
                let id = cpu.id;
                // A stale cache may name CPUs that no longer exist: ignore those.
                for member in field(cpu).intersection(&all).iter() {
                    let (a, b) = (find(&leader, id), find(&leader, member));
                    if a != b {
                        leader.insert(a.max(b), a.min(b));
                    }
                }
            }
            let mut groups: BTreeMap<usize, CpuSet> = BTreeMap::new();
            for id in all.iter() {
                groups.entry(find(&leader, id)).or_default().insert(id);
            }
            for cpu in self.cpus.iter_mut() {
                *field(cpu) = groups[&find(&leader, cpu.id)].clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::fake_sys;

    #[test]
    fn cache_preserves_topology_of_offline_cpus() {
        let f = fake_sys();
        let cache_path = f.dir.path().join("state/topology.toml");
        let before = f.discover();
        TopologyCache::save(&cache_path, &before).unwrap();

        // Offline an SMT sibling (cpu2) and an LP-E core (cpu20).
        f.offline(2);
        f.offline(20);

        // Without the cache the facts are lost (the kernel even drops an
        // offline CPU from the hybrid PMU lists, so its type is a guess)...
        let blind = Topology::discover(&f.root(), &f.devices(), None).unwrap();
        assert!(!blind.get(2).unwrap().topology_known);
        assert!(!blind.get(20).unwrap().topology_known);
        assert_eq!(blind.get(20).unwrap().kind, CoreKind::P);

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
        f.offline(2);
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
        f.offline(2);
        let topo = Topology::discover(&f.root(), &f.devices(), Some(&cache)).unwrap();
        assert!(!topo.get(2).unwrap().topology_known);
    }

    /// What `Ctx::discover` does on every run: load the cache, look, save.
    fn run_once(f: &crate::fake::Fake, cache_path: &Path) -> Topology {
        let cache = TopologyCache::load(cache_path);
        let topo = Topology::discover(&f.root(), &f.devices(), cache.as_ref()).unwrap();
        TopologyCache::save(cache_path, &topo).unwrap();
        topo
    }

    fn set(s: &str) -> CpuSet {
        CpuSet::parse_list(s).unwrap()
    }

    #[test]
    fn discovers_the_whole_hybrid_layout() {
        let f = fake_sys();
        let t = f.discover();
        assert_eq!(t.cpus.len(), 22);
        assert_eq!(t.all(), set("0-21"));
        assert_eq!(t.online(), set("0-21"));
        assert_eq!(t.of_kind(CoreKind::P), set("0-11"));
        assert_eq!(t.of_kind(CoreKind::E), set("12-19"));
        // In cpu_atom's list but without an L3: the low-power cores.
        assert_eq!(t.of_kind(CoreKind::LpE), set("20-21"));
        assert!(t.cpus.iter().all(|c| c.topology_known && c.online && c.package == 0));
        assert!(t.cpus.windows(2).all(|w| w[0].id < w[1].id));

        let c0 = t.get(0).unwrap();
        assert!(!c0.hotpluggable, "cpu0 has no online file");
        assert_eq!((c0.core_id, &c0.siblings, &c0.l2_group), (16, &set("0,5"), &set("0,5")));
        assert!(t.cpus.iter().skip(1).all(|c| c.hotpluggable));

        let c13 = t.get(13).unwrap();
        assert_eq!((c13.core_id, &c13.siblings, &c13.l2_group), (1, &set("13"), &set("12-15")));
        assert_eq!((c13.min_khz, c13.max_khz, c13.cur_khz), (Some(400_000), Some(3_800_000), Some(1_200_000)));
        assert_eq!((c13.scaling_min_khz, c13.scaling_max_khz), (Some(400_000), Some(3_800_000)));
        assert_eq!((c13.governor.as_deref(), c13.epp.as_deref()), (Some("powersave"), Some("balance_performance")));
        assert_eq!(t.get(1).unwrap().max_khz, Some(4_800_000));
        assert_eq!(t.get(21).unwrap().l2_group, set("20-21"));
        assert!(t.get(22).is_none());
    }

    #[test]
    fn smt_threads_are_every_thread_but_the_first_of_each_core() {
        let t = fake_sys().discover();
        assert_eq!(t.smt_secondaries(), set("2,4-5,7,9,11"));
        assert!(!t.get(0).unwrap().is_secondary_thread());
        assert!(t.get(5).unwrap().is_secondary_thread()); // cpu0's sibling
        assert!(!t.get(12).unwrap().is_secondary_thread()); // E-cores have one thread
    }

    #[test]
    fn a_cpu_without_hybrid_pmus_is_all_p_cores() {
        let f = fake_sys();
        fs::remove_dir_all(f.devices().join("cpu_core")).unwrap();
        fs::remove_dir_all(f.devices().join("cpu_atom")).unwrap();
        let t = f.discover();
        assert_eq!(t.of_kind(CoreKind::P), t.all());
    }

    #[test]
    fn the_low_power_pmu_wins_over_the_l3_heuristic() {
        let f = fake_sys();
        fs::create_dir_all(f.devices().join("cpu_lowpower")).unwrap();
        fs::write(f.devices().join("cpu_lowpower/cpus"), "19\n").unwrap();
        let t = f.discover();
        assert_eq!(t.get(19).unwrap().kind, CoreKind::LpE); // has an L3, listed explicitly
        assert_eq!(t.of_kind(CoreKind::LpE), set("19-21"));
    }

    #[test]
    fn atom_cores_without_l3_are_plain_e_cores_when_there_are_no_p_cores() {
        let f = fake_sys();
        fs::remove_dir_all(f.devices().join("cpu_core")).unwrap();
        let t = f.discover();
        assert_eq!(t.get(20).unwrap().kind, CoreKind::E);
        assert!(t.of_kind(CoreKind::LpE).is_empty());
    }

    #[test]
    fn cpu_list_comes_from_present_then_possible() {
        let f = fake_sys();
        fs::rename(f.root().join("present"), f.root().join("possible")).unwrap();
        assert_eq!(f.discover().cpus.len(), 22);

        fs::remove_file(f.root().join("possible")).unwrap();
        let err = Topology::discover(&f.root(), &f.devices(), None).unwrap_err();
        assert!(err.to_string().contains(&f.root().display().to_string()), "{err}");

        f.write("present", "0-zz");
        assert!(Topology::discover(&f.root(), &f.devices(), None).is_err());
    }

    #[test]
    fn listed_cpus_without_a_directory_are_skipped() {
        let f = fake_sys();
        f.write("present", "0-31"); // sockets for CPUs that are not there
        assert_eq!(f.discover().all(), set("0-21"));
    }

    #[test]
    fn a_tree_without_any_cpu_is_an_error_not_an_empty_topology() {
        // An empty topology would make every UI index out of bounds.
        let f = fake_sys();
        f.write("present", "");
        let err = Topology::discover(&f.root(), &f.devices(), None).unwrap_err();
        assert!(err.to_string().contains("no CPUs found"), "{err}");
        f.write("present", "40-41");
        assert!(Topology::discover(&f.root(), &f.devices(), None).is_err());
    }

    #[test]
    fn an_offline_cpu_without_cache_is_marked_as_guesswork() {
        let f = fake_sys();
        f.offline(12);
        let t = f.discover();
        let c = t.get(12).unwrap();
        assert!(!c.online && c.hotpluggable && !c.topology_known);
        // Placeholders: its own id as core, alone in every group.
        assert_eq!((c.core_id, &c.siblings, &c.l2_group), (12, &set("12"), &set("12")));
        assert_eq!((c.max_khz, c.scaling_max_khz, c.cur_khz, c.governor.clone()), (None, None, None, None));
        assert_eq!(t.online(), set("0-11,13-21"));
    }

    #[test]
    fn groups_stay_whole_when_the_kernel_shrinks_the_live_lists() {
        let f = fake_sys();
        let cache = f.dir.path().join("topology.toml");
        run_once(&f, &cache);

        // Offline the *first* thread of core 8: the kernel now reports cpu2
        // as the only thread of its core, and cpu13 drops out of the L2 lists.
        f.offline(1);
        f.offline(13);
        assert_eq!(f.read("cpu2/topology/thread_siblings_list"), "2");
        assert_eq!(f.read("cpu12/cache/index2/shared_cpu_list"), "12,14-15");

        let t = run_once(&f, &cache);
        assert_eq!(t.get(2).unwrap().siblings, set("1-2"));
        assert_eq!(t.get(1).unwrap().siblings, set("1-2"));
        assert!(t.get(2).unwrap().is_secondary_thread(), "cpu2 must not turn into a primary thread");
        assert_eq!(t.smt_secondaries(), set("2,4-5,7,9,11"));
        for id in 12..16 {
            assert_eq!(t.get(id).unwrap().l2_group, set("12-15"), "cpu{id}");
        }
    }

    #[test]
    fn the_cache_survives_any_order_of_offlining_and_onlining() {
        let f = fake_sys();
        let cache = f.dir.path().join("topology.toml");
        run_once(&f, &cache);
        // Each run saves what it saw; shrunken live lists must never be what
        // ends up remembered for a CPU.
        f.offline(2);
        run_once(&f, &cache);
        f.online(2);
        f.offline(1);
        run_once(&f, &cache);
        f.offline(2);
        let t = run_once(&f, &cache);
        for id in [1, 2] {
            let c = t.get(id).unwrap();
            assert!(!c.online && c.topology_known);
            assert_eq!((c.core_id, &c.siblings, c.kind), (8, &set("1-2"), CoreKind::P), "cpu{id}");
        }
        assert_eq!(t.smt_secondaries(), set("2,4-5,7,9,11"));
        // And once everything is back the cache matches a fresh look.
        f.online(1);
        f.online(2);
        let t = run_once(&f, &cache);
        assert!(t.cpus.iter().all(|c| c.online && c.topology_known));
        assert_eq!(t.get(1).unwrap().siblings, set("1-2"));
    }

    #[test]
    fn cached_limits_fill_in_for_offline_cpus() {
        let f = fake_sys();
        let cache = f.dir.path().join("topology.toml");
        run_once(&f, &cache);
        f.offline(20);
        let c = run_once(&f, &cache).get(20).cloned().unwrap();
        assert_eq!((c.kind, c.min_khz, c.max_khz), (CoreKind::LpE, Some(400_000), Some(2_500_000)));
        assert_eq!((c.scaling_max_khz, c.governor), (None, None)); // live values stay unknown
    }

    #[test]
    fn cache_entries_naming_cpus_that_do_not_exist_are_ignored() {
        let f = fake_sys();
        let path = f.dir.path().join("topology.toml");
        TopologyCache::save(&path, &f.discover()).unwrap();
        let mut cache = TopologyCache::load(&path).unwrap();
        cache.cpu.get_mut("2").unwrap().siblings.push(99);
        cache.cpu.get_mut("2").unwrap().l2_group.push(99);
        f.offline(2);
        let t = Topology::discover(&f.root(), &f.devices(), Some(&cache)).unwrap();
        assert_eq!(t.get(2).unwrap().siblings, set("1-2"));
        assert_eq!(t.get(2).unwrap().l2_group, set("1-2"));
    }

    #[test]
    fn an_unchanged_cache_file_is_left_alone() {
        use std::os::unix::fs::MetadataExt;
        let f = fake_sys();
        let path = f.dir.path().join("state/topology.toml");
        let topo = f.discover();
        TopologyCache::save(&path, &topo).unwrap();
        let first = fs::metadata(&path).unwrap().ino();
        // The UIs call this every second: no new file may appear each time.
        TopologyCache::save(&path, &topo).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), first);
        // A real change is written (as a fresh file renamed into place).
        let mut changed = topo.clone();
        changed.cpus[3].core_id = 99;
        TopologyCache::save(&path, &changed).unwrap();
        assert_eq!(TopologyCache::load(&path).unwrap().cpu["3"].core_id, 99);
    }

    #[test]
    fn a_missing_or_corrupt_cache_loads_as_none() {
        let d = tempfile::tempdir().unwrap();
        assert!(TopologyCache::load(&d.path().join("nope.toml")).is_none());
        let bad = d.path().join("bad.toml");
        fs::write(&bad, "present = \"0-3\"\n[cpu.0\nkind = ").unwrap();
        assert!(TopologyCache::load(&bad).is_none());
        // ...and saving over a corrupt file replaces it with a good one.
        let f = fake_sys();
        TopologyCache::save(&bad, &f.discover()).unwrap();
        assert_eq!(TopologyCache::load(&bad).unwrap().cpu.len(), 22);
    }

    #[test]
    fn globals_read_the_intel_pstate_knobs() {
        let f = fake_sys();
        let t = f.discover();
        let g = Globals::read(&f.root(), &t);
        assert_eq!(g.smt_control.as_deref(), Some("on"));
        assert_eq!(g.no_turbo, Some(false));
        assert_eq!(g.pstate_status.as_deref(), Some("active"));
        assert_eq!(g.available_governors, ["performance", "powersave"]);
        assert_eq!(g.available_epp, ["default", "performance", "balance_performance", "balance_power", "power"]);
        f.write("intel_pstate/no_turbo", "1");
        assert_eq!(Globals::read(&f.root(), &t).no_turbo, Some(true));
    }

    #[test]
    fn globals_fall_back_to_the_generic_boost_flag() {
        let f = fake_sys();
        let t = f.discover();
        fs::remove_dir_all(f.root().join("intel_pstate")).unwrap();
        fs::remove_dir_all(f.root().join("smt")).unwrap();
        let g = Globals::read(&f.root(), &t);
        assert_eq!((g.no_turbo, g.smt_control, g.pstate_status), (None, None, None));
        // `boost` is "1 = on", the opposite of `no_turbo`.
        f.write("cpufreq/boost", "1");
        assert_eq!(Globals::read(&f.root(), &t).no_turbo, Some(false));
        f.write("cpufreq/boost", "0");
        assert_eq!(Globals::read(&f.root(), &t).no_turbo, Some(true));
    }

    #[test]
    fn globals_take_governor_lists_from_any_online_cpu() {
        let f = fake_sys();
        fs::remove_dir_all(f.root().join("cpu0/cpufreq")).unwrap();
        let t = f.discover();
        assert_eq!(Globals::read(&f.root(), &t).available_governors, ["performance", "powersave"]);
        for c in &t.cpus {
            let _ = fs::remove_dir_all(f.root().join(format!("cpu{}/cpufreq", c.id)));
        }
        let g = Globals::read(&f.root(), &t);
        assert!(g.available_governors.is_empty() && g.available_epp.is_empty());
    }

    #[test]
    fn small_helpers() {
        assert_eq!([CoreKind::P, CoreKind::E, CoreKind::LpE].map(|k| k.to_string()), ["P", "E", "LP-E"]);
        assert_eq!(cpu_dir(Path::new("/sys/x"), 7), Path::new("/sys/x/cpu7"));
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("v"), "  42 \n").unwrap();
        assert_eq!(read_trim(d.path().join("v")).as_deref(), Some("42"));
        assert_eq!(read_u64(d.path().join("v")), Some(42));
        assert_eq!(read_trim(d.path().join("missing")), None);
        fs::write(d.path().join("v"), "-1\n").unwrap();
        assert_eq!(read_u64(d.path().join("v")), None);
    }
}
