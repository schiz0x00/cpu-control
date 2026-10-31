//! Test-only fake sysfs tree modelled on an Intel Core Ultra 7 155H
//! (6 P-cores w/ SMT, 8 E-cores, 2 LP-E cores; cpu0 is not hot-pluggable).

use crate::topology::Topology;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct Fake {
    pub dir: TempDir,
}

impl Fake {
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("system/cpu")
    }
    pub fn devices(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }
    pub fn discover(&self) -> Topology {
        Topology::discover(&self.root(), &self.devices(), None).unwrap()
    }
    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.root().join(rel)).unwrap().trim().to_string()
    }
}

fn w(path: &Path, s: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("{s}\n")).unwrap();
}

pub fn fake_sys() -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let f = Fake { dir };
    let root = f.root();
    let dev = f.devices();
    w(&root.join("present"), "0-21");
    w(&root.join("smt/control"), "on");
    w(&root.join("intel_pstate/no_turbo"), "0");
    w(&root.join("intel_pstate/status"), "active");
    w(&dev.join("cpu_core/cpus"), "0-11");
    w(&dev.join("cpu_atom/cpus"), "12-21");

    // (cpu, core_id, siblings, l2, max_khz)
    let mut spec: Vec<(usize, usize, String, &str, u64)> = vec![
        (0, 16, "0,5".to_string(), "0,5", 4_500_000),
        (1, 8, "1-2".to_string(), "1-2", 4_800_000),
        (2, 8, "1-2".to_string(), "1-2", 4_800_000),
        (3, 12, "3-4".to_string(), "3-4", 4_800_000),
        (4, 12, "3-4".to_string(), "3-4", 4_800_000),
        (5, 16, "0,5".to_string(), "0,5", 4_500_000),
        (6, 20, "6-7".to_string(), "6-7", 4_500_000),
        (7, 20, "6-7".to_string(), "6-7", 4_500_000),
        (8, 24, "8-9".to_string(), "8-9", 4_500_000),
        (9, 24, "8-9".to_string(), "8-9", 4_500_000),
        (10, 28, "10-11".to_string(), "10-11", 4_500_000),
        (11, 28, "10-11".to_string(), "10-11", 4_500_000),
    ];
    for i in 12..20 {
        let l2 = if i < 16 { "12-15" } else { "16-19" };
        spec.push((i, i - 12, i.to_string(), l2, 3_800_000));
    }
    spec.push((20, 32, "20".to_string(), "20-21", 2_500_000));
    spec.push((21, 33, "21".to_string(), "20-21", 2_500_000));

    for (id, core, sib, l2, max) in spec {
        let d = root.join(format!("cpu{id}"));
        if id != 0 {
            w(&d.join("online"), "1");
        }
        w(&d.join("topology/core_id"), &core.to_string());
        w(&d.join("topology/physical_package_id"), "0");
        w(&d.join("topology/thread_siblings_list"), &sib);
        w(&d.join("cache/index2/level"), "2");
        w(&d.join("cache/index2/type"), "Unified");
        w(&d.join("cache/index2/shared_cpu_list"), l2);
        if id < 20 {
            w(&d.join("cache/index3/level"), "3");
            w(&d.join("cache/index3/type"), "Unified");
            w(&d.join("cache/index3/shared_cpu_list"), "0-19");
        }
        let cf = d.join("cpufreq");
        w(&cf.join("cpuinfo_min_freq"), "400000");
        w(&cf.join("cpuinfo_max_freq"), &max.to_string());
        w(&cf.join("scaling_cur_freq"), "1200000");
        w(&cf.join("scaling_min_freq"), "400000");
        w(&cf.join("scaling_max_freq"), &max.to_string());
        w(&cf.join("scaling_governor"), "powersave");
        w(&cf.join("scaling_available_governors"), "performance powersave");
        w(&cf.join("energy_performance_preference"), "balance_performance");
        w(
            &cf.join("energy_performance_available_preferences"),
            "default performance balance_performance balance_power power",
        );
    }
    f
}

pub fn fake_meteor_lake() -> (Fake, Topology) {
    let f = fake_sys();
    let t = f.discover();
    (f, t)
}
