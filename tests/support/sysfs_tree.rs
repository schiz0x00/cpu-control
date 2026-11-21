//! A fake sysfs CPU tree for tests, modelled on an Intel Core Ultra 7 155H
//! (6 P-cores w/ SMT, 8 E-cores, 2 LP-E cores; cpu0 is not hot-pluggable).
//!
//! Shared by the unit tests (through `src/fake.rs`) and the CLI tests, so it
//! depends on nothing but std and tempfile.
//!
//! The tree also plays the kernel's part: [`Fake::settle`] applies the side
//! effects the real kernel has when a CPU's `online` file changes.

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Spec {
    id: usize,
    core_id: usize,
    siblings: Vec<usize>,
    l2: Vec<usize>,
    l3: Vec<usize>,
    max_khz: u64,
    /// Hybrid PMU directory under the devices root listing this CPU.
    pmu: &'static str,
}

pub struct Fake {
    pub dir: TempDir,
    cpus: Vec<Spec>,
}

fn w(path: &Path, s: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("{s}\n")).unwrap();
}

/// Kernel list format with ranges, e.g. `0-3,8`.
fn list(ids: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
            j += 1;
        }
        out.push(if j == i { ids[i].to_string() } else { format!("{}-{}", ids[i], ids[j]) });
        i = j + 1;
    }
    out.join(",")
}

impl Fake {
    /// The fake `/sys/devices/system/cpu`.
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("system/cpu")
    }

    /// The fake `/sys/devices`.
    pub fn devices(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Trimmed contents of a file under [`Fake::root`].
    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.root().join(rel)).unwrap().trim().to_string()
    }

    /// Overwrite a file under [`Fake::root`] (creating it if needed).
    pub fn write(&self, rel: &str, s: &str) {
        w(&self.root().join(rel), s);
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.root().join(rel).exists()
    }

    fn is_online(&self, id: usize) -> bool {
        let f = self.root().join(format!("cpu{id}/online"));
        !f.exists() || fs::read_to_string(f).unwrap().trim() == "1"
    }

    /// Take a CPU offline the way the kernel does (see [`Fake::settle`]).
    pub fn offline(&self, id: usize) {
        self.write(&format!("cpu{id}/online"), "0");
        self.settle();
    }

    /// Bring a CPU back online the way the kernel does.
    pub fn online(&self, id: usize) {
        self.write(&format!("cpu{id}/online"), "1");
        self.settle();
    }

    /// Do what the kernel does after `online` files change:
    /// - an offline CPU loses its `topology/`, `cache/` and `cpufreq/` entries;
    /// - it disappears from its relatives' sibling and shared-cache lists and
    ///   from the hybrid PMU `cpus` lists;
    /// - a CPU coming back gets all of that again, with cpufreq at defaults.
    pub fn settle(&self) {
        let root = self.root();
        let online: Vec<usize> = self.cpus.iter().map(|c| c.id).filter(|&id| self.is_online(id)).collect();
        let live = |ids: &[usize]| list(&ids.iter().copied().filter(|i| online.contains(i)).collect::<Vec<_>>());
        for c in &self.cpus {
            let d = root.join(format!("cpu{}", c.id));
            if !online.contains(&c.id) {
                for sub in ["topology", "cache", "cpufreq"] {
                    let _ = fs::remove_dir_all(d.join(sub));
                }
                continue;
            }
            w(&d.join("topology/core_id"), &c.core_id.to_string());
            w(&d.join("topology/physical_package_id"), "0");
            w(&d.join("topology/thread_siblings_list"), &live(&c.siblings));
            // A lower-level cache as well, so "find the L2" has something to skip.
            w(&d.join("cache/index0/level"), "1");
            w(&d.join("cache/index0/type"), "Instruction");
            w(&d.join("cache/index0/shared_cpu_list"), &live(&c.siblings));
            w(&d.join("cache/index2/level"), "2");
            w(&d.join("cache/index2/type"), "Unified");
            w(&d.join("cache/index2/shared_cpu_list"), &live(&c.l2));
            if !c.l3.is_empty() {
                w(&d.join("cache/index3/level"), "3");
                w(&d.join("cache/index3/type"), "Unified");
                w(&d.join("cache/index3/shared_cpu_list"), &live(&c.l3));
            }
            let cf = d.join("cpufreq");
            if !cf.exists() {
                w(&cf.join("cpuinfo_min_freq"), "400000");
                w(&cf.join("cpuinfo_max_freq"), &c.max_khz.to_string());
                w(&cf.join("scaling_cur_freq"), "1200000");
                w(&cf.join("scaling_min_freq"), "400000");
                w(&cf.join("scaling_max_freq"), &c.max_khz.to_string());
                w(&cf.join("scaling_governor"), "powersave");
                w(&cf.join("scaling_available_governors"), "performance powersave");
                w(&cf.join("energy_performance_preference"), "balance_performance");
                w(
                    &cf.join("energy_performance_available_preferences"),
                    "default performance balance_performance balance_power power",
                );
            }
        }
        for pmu in ["cpu_core", "cpu_atom"] {
            let file = self.devices().join(pmu).join("cpus");
            // A test may delete the PMU directories to model a non-hybrid CPU.
            if file.exists() {
                let members: Vec<usize> = self.cpus.iter().filter(|c| c.pmu == pmu).map(|c| c.id).collect();
                w(&file, &live(&members));
            }
        }
    }
}

pub fn fake_sys() -> Fake {
    let mut cpus = Vec::new();
    // P-cores: (core_id, its two threads, max frequency).
    for (core_id, threads, max_khz) in [
        (16, [0, 5], 4_500_000),
        (8, [1, 2], 4_800_000),
        (12, [3, 4], 4_800_000),
        (20, [6, 7], 4_500_000),
        (24, [8, 9], 4_500_000),
        (28, [10, 11], 4_500_000),
    ] {
        for id in threads {
            cpus.push(Spec {
                id,
                core_id,
                siblings: threads.to_vec(),
                l2: threads.to_vec(),
                l3: (0..20).collect(),
                max_khz,
                pmu: "cpu_core",
            });
        }
    }
    // E-cores: two clusters of four sharing an L2.
    for id in 12..20 {
        let l2 = if id < 16 { 12..16 } else { 16..20 };
        cpus.push(Spec {
            id,
            core_id: id - 12,
            siblings: vec![id],
            l2: l2.collect(),
            l3: (0..20).collect(),
            max_khz: 3_800_000,
            pmu: "cpu_atom",
        });
    }
    // LP-E cores: on the SoC tile, no L3.
    for id in 20..22 {
        cpus.push(Spec { id, core_id: id + 12, siblings: vec![id], l2: vec![20, 21], l3: vec![], max_khz: 2_500_000, pmu: "cpu_atom" });
    }
    cpus.sort_by_key(|c| c.id);

    let f = Fake { dir: tempfile::tempdir().unwrap(), cpus };
    let root = f.root();
    w(&root.join("present"), "0-21");
    w(&root.join("smt/control"), "on");
    w(&root.join("intel_pstate/no_turbo"), "0");
    w(&root.join("intel_pstate/status"), "active");
    w(&f.devices().join("cpu_core/cpus"), "");
    w(&f.devices().join("cpu_atom/cpus"), "");
    for c in &f.cpus {
        // cpu0 has no `online` file: the boot CPU cannot be unplugged.
        if c.id != 0 {
            w(&root.join(format!("cpu{}/online", c.id)), "1");
        } else {
            fs::create_dir_all(root.join("cpu0")).unwrap();
        }
    }
    f.settle();
    f
}
