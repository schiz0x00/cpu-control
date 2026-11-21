//! Runtime context shared by the CLI and the TUI: where sysfs lives, where
//! config/cache files go, and how requests get executed.

use crate::plan::{self, Plan, Request};
use crate::topology::{Globals, Topology, TopologyCache};
use anyhow::Result;
use std::env;
use std::fs;
use std::os::unix::fs::lchown;
use std::path::{Component, Path, PathBuf};

pub const SYS_CPU: &str = "/sys/devices/system/cpu";
pub const SYS_DEVICES: &str = "/sys/devices";

#[derive(Clone)]
pub struct Ctx {
    pub root: PathBuf,
    pub devices: PathBuf,
    pub dry_run: bool,
    pub profiles_path: PathBuf,
    cache_paths: Vec<PathBuf>,
}

#[derive(Default)]
pub struct Outcome {
    pub warnings: Vec<String>,
    /// Descriptions of every action in the plan (executed or, in dry-run, skipped).
    pub actions: Vec<String>,
    pub failures: Vec<String>,
    pub applied: usize,
    pub hint: Option<String>,
    pub dry_run: bool,
}

impl Ctx {
    pub fn new(sysfs_root: Option<PathBuf>, profiles: Option<PathBuf>, dry_run: bool) -> Ctx {
        let custom = sysfs_root.is_some();
        let root = sysfs_root.unwrap_or_else(|| PathBuf::from(SYS_CPU));
        // With a custom root, `/sys/devices` is two levels up (devices/system/cpu).
        let devices = if custom {
            root.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(|| root.clone())
        } else {
            PathBuf::from(SYS_DEVICES)
        };
        let cache_paths = match env::var_os("CPU_CONTROL_STATE_DIR") {
            Some(d) => vec![PathBuf::from(d).join("topology.toml")],
            None if custom => vec![],
            None => {
                let mut v = vec![PathBuf::from("/var/lib/cpu-control/topology.toml")];
                if let Some(h) = user_home() {
                    v.push(h.join(".local/state/cpu-control/topology.toml"));
                }
                v
            }
        };
        let profiles_path = profiles
            .or_else(|| env::var_os("CPU_CONTROL_PROFILES").map(PathBuf::from))
            .or_else(|| user_home().map(|h| h.join(".config/cpu-control/profiles.toml")))
            .unwrap_or_else(|| PathBuf::from("cpu-control-profiles.toml"));
        Ctx { root, devices, dry_run, profiles_path, cache_paths }
    }

    /// Read topology + global knobs, using and refreshing the offline-CPU cache.
    pub fn discover(&self) -> Result<(Topology, Globals)> {
        let cache = self.cache_paths.iter().find_map(|p| TopologyCache::load(p));
        let topo = Topology::discover(&self.root, &self.devices, cache.as_ref())?;
        // Best effort: remember what online CPUs look like for later. A dry run
        // promises to write nothing, and that includes the cache.
        if !self.dry_run {
            for p in &self.cache_paths {
                if TopologyCache::save(p, &topo).is_ok() {
                    break;
                }
            }
        }
        let globals = Globals::read(&self.root, &topo);
        Ok((topo, globals))
    }

    pub fn plan(&self, topo: &Topology, req: &Request) -> Result<Plan> {
        plan::build_plan(topo, &self.root, req)
    }

    /// Plan and (unless dry-run) apply a request.
    pub fn execute(&self, topo: &Topology, req: &Request) -> Result<Outcome> {
        let plan = self.plan(topo, req)?;
        let mut out = Outcome {
            warnings: plan.warnings.clone(),
            actions: plan.actions.iter().map(|a| a.desc.clone()).collect(),
            dry_run: self.dry_run,
            ..Default::default()
        };
        if !self.dry_run {
            let report = plan::apply(&plan);
            out.applied = report.applied;
            out.hint = plan::permission_hint(&report).map(String::from);
            out.failures = report.failed.iter().map(|(a, e)| format!("{}: {e}", a.desc)).collect();
        }
        Ok(out)
    }
}

/// Home of the invoking user, even under `sudo`.
fn user_home() -> Option<PathBuf> {
    if let Ok(user) = env::var("SUDO_USER")
        && user != "root"
        && let Ok(passwd) = fs::read_to_string("/etc/passwd")
        && let Some(home) = passwd_home(&passwd, &user)
    {
        return Some(home);
    }
    env::var_os("HOME").map(PathBuf::from)
}

/// Home directory of `user` in passwd(5) text.
fn passwd_home(passwd: &str, user: &str) -> Option<PathBuf> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 6 && f[0] == user && !f[5].is_empty()).then(|| PathBuf::from(f[5]))
    })
}

/// When running under sudo, hand files we created back to the invoking user.
///
/// Uses `lchown`, which never follows a symlink: whatever sits at these paths
/// by the time we get here, ownership of a file elsewhere cannot change.
pub fn fix_ownership(path: &Path) {
    let (Some(uid), Some(gid)) = (
        env::var("SUDO_UID").ok().and_then(|s| s.parse::<u32>().ok()),
        env::var("SUDO_GID").ok().and_then(|s| s.parse::<u32>().ok()),
    ) else {
        return;
    };
    let _ = lchown(path, Some(uid), Some(gid));
    let Some(home) = user_home() else { return };
    for dir in dirs_inside_home(path, &home) {
        let _ = lchown(dir, Some(uid), Some(gid));
    }
}

/// Parent directories of `path` that we may have created and so hand back:
/// only those strictly inside `home`, never the home itself or anything
/// outside it.
fn dirs_inside_home(path: &Path, home: &Path) -> Vec<PathBuf> {
    // `starts_with` compares components without resolving them, so
    // `~/../../etc/x` "starts with" the home while naming /etc. Refuse any
    // path that is not plainly absolute rather than chown the wrong tree.
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Vec::new();
    }
    let mut dirs = Vec::new();
    let mut cur = path.parent();
    while let Some(p) = cur {
        if p == home || !p.starts_with(home) {
            break;
        }
        dirs.push(p.to_path_buf());
        cur = p.parent();
    }
    dirs
}

/// True when running with an effective uid of 0.
pub fn is_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(2).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
impl Ctx {
    /// A context over a fake tree that owes nothing to the process environment.
    pub fn for_test(f: &crate::fake::Fake, dry_run: bool) -> Ctx {
        Ctx {
            root: f.root(),
            devices: f.devices(),
            dry_run,
            profiles_path: f.dir.path().join("profiles.toml"),
            cache_paths: vec![f.dir.path().join("state/topology.toml")],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpuset::CpuSet;
    use crate::fake::fake_sys;
    use crate::plan::Tune;

    fn cpus(s: &str) -> CpuSet {
        CpuSet::parse_list(s).unwrap()
    }

    #[test]
    fn defaults_point_at_the_real_sysfs() {
        let ctx = Ctx::new(None, Some("p.toml".into()), false);
        assert_eq!((ctx.root.as_path(), ctx.devices.as_path()), (Path::new(SYS_CPU), Path::new(SYS_DEVICES)));
        assert_eq!(ctx.profiles_path, Path::new("p.toml"));
        assert!(!ctx.dry_run);
    }

    #[test]
    fn a_custom_root_implies_its_devices_directory() {
        let ctx = Ctx::new(Some("/tmp/fake/system/cpu".into()), Some("/x/p.toml".into()), true);
        assert_eq!(ctx.devices, Path::new("/tmp/fake"));
        assert!(ctx.dry_run);
        // Too shallow to have a grandparent: fall back to the root itself.
        assert_eq!(Ctx::new(Some("/".into()), Some("p".into()), false).devices, Path::new("/"));
    }

    #[test]
    fn discover_records_the_topology_for_later_runs() {
        let f = fake_sys();
        let ctx = Ctx::for_test(&f, false);
        let (topo, globals) = ctx.discover().unwrap();
        assert_eq!(topo.cpus.len(), 22);
        assert_eq!(globals.no_turbo, Some(false));
        assert!(ctx.cache_paths[0].exists());

        f.offline(2);
        f.offline(20);
        let (topo, _) = ctx.discover().unwrap();
        let c2 = topo.get(2).unwrap();
        assert!(c2.topology_known && c2.is_secondary_thread());
        assert_eq!(topo.get(20).unwrap().kind, crate::topology::CoreKind::LpE);
    }

    #[test]
    fn a_dry_run_reads_the_cache_but_never_writes_it() {
        let f = fake_sys();
        let dry = Ctx::for_test(&f, true);
        dry.discover().unwrap();
        assert!(!dry.cache_paths[0].exists(), "dry run created the cache");
        assert!(!dry.cache_paths[0].parent().unwrap().exists());

        Ctx::for_test(&f, false).discover().unwrap();
        f.offline(2);
        assert!(dry.discover().unwrap().0.get(2).unwrap().topology_known);
    }

    #[test]
    fn the_first_writable_cache_location_is_used() {
        let f = fake_sys();
        let blocker = f.dir.path().join("blocker");
        fs::write(&blocker, "a file, so nothing can be created beneath it").unwrap();
        let mut ctx = Ctx::for_test(&f, false);
        let good = ctx.cache_paths[0].clone();
        let unused = f.dir.path().join("later/topology.toml");
        ctx.cache_paths = vec![blocker.join("topology.toml"), good.clone(), unused.clone()];
        ctx.discover().unwrap();
        assert!(good.exists() && !unused.exists());
        // And it is found again even though the first location cannot be read.
        f.offline(2);
        assert!(ctx.discover().unwrap().0.get(2).unwrap().topology_known);
    }

    #[test]
    fn discovery_works_without_any_cache_location() {
        let f = fake_sys();
        let mut ctx = Ctx::for_test(&f, false);
        ctx.cache_paths.clear();
        assert_eq!(ctx.discover().unwrap().0.cpus.len(), 22);
        fs::remove_file(f.root().join("present")).unwrap();
        assert!(ctx.discover().is_err());
    }

    #[test]
    fn execute_in_a_dry_run_reports_but_does_not_touch_anything() {
        let f = fake_sys();
        let ctx = Ctx::for_test(&f, true);
        let (topo, _) = ctx.discover().unwrap();
        let req = Request { offline: cpus("0,12-13"), turbo: Some(false), ..Default::default() };
        let out = ctx.execute(&topo, &req).unwrap();
        assert!(out.dry_run);
        assert_eq!(out.actions, ["take cpu12 offline", "take cpu13 offline", "turbo boost disabled"]);
        assert_eq!((out.applied, out.failures.len(), out.hint.is_some()), (0, 0, false));
        assert!(out.warnings.iter().any(|w| w.contains("cpu0 cannot be taken offline")));
        assert_eq!((f.read("cpu12/online"), f.read("intel_pstate/no_turbo")), ("1".to_string(), "0".to_string()));
    }

    #[test]
    fn execute_applies_and_reports_failures_by_description() {
        let f = fake_sys();
        let ctx = Ctx::for_test(&f, false);
        let (topo, _) = ctx.discover().unwrap();
        fs::remove_file(f.root().join("cpu13/cpufreq/scaling_governor")).unwrap();
        let req = Request {
            offline: cpus("20"),
            tune: vec![Tune { cpus: cpus("12-13"), governor: Some("performance".into()), ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(ctx.plan(&topo, &req).unwrap().actions.len(), 3);
        let out = ctx.execute(&topo, &req).unwrap();
        assert!(!out.dry_run);
        assert_eq!((out.applied, out.actions.len(), out.failures.len()), (2, 3, 1));
        assert!(out.failures[0].starts_with("cpu13 governor -> performance: "), "{}", out.failures[0]);
        assert_eq!(out.hint, None); // a missing file is not a permission problem
        assert_eq!((f.read("cpu20/online"), f.read("cpu12/cpufreq/scaling_governor")), ("0".to_string(), "performance".to_string()));
    }

    #[test]
    fn execute_refuses_an_invalid_request_before_writing() {
        let f = fake_sys();
        let ctx = Ctx::for_test(&f, false);
        let (topo, _) = ctx.discover().unwrap();
        let req = Request { offline: cpus("12"), online: cpus("40"), ..Default::default() };
        assert!(ctx.execute(&topo, &req).is_err());
        assert_eq!(f.read("cpu12/online"), "1");
    }

    #[test]
    fn passwd_lookup() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      # a comment\n\
                      broken:line\n\
                      nohome:x:5:5:::/bin/sh\n\
                      ann:x:1000:1000:Ann,,,:/home/ann:/bin/zsh\n\
                      annie:x:1001:1001::/home/annie:/bin/sh\n";
        assert_eq!(passwd_home(passwd, "ann"), Some(PathBuf::from("/home/ann")));
        assert_eq!(passwd_home(passwd, "annie"), Some(PathBuf::from("/home/annie")));
        assert_eq!(passwd_home(passwd, "an"), None); // whole-name match only
        assert_eq!(passwd_home(passwd, "broken"), None);
        assert_eq!(passwd_home(passwd, "nohome"), None);
        assert_eq!(passwd_home("", "ann"), None);
    }

    #[test]
    fn only_directories_strictly_inside_home_are_handed_back() {
        let home = Path::new("/home/ann");
        let dirs = |p: &str| dirs_inside_home(Path::new(p), home);
        assert_eq!(
            dirs("/home/ann/.config/cpu-control/profiles.toml"),
            [Path::new("/home/ann/.config/cpu-control"), Path::new("/home/ann/.config")]
        );
        assert!(dirs("/home/ann/profiles.toml").is_empty()); // the home itself is never touched
        assert!(dirs("/etc/cpu-control/profiles.toml").is_empty());
        assert!(dirs("/home/annie/.config/p.toml").is_empty()); // a name that merely starts the same
        assert!(dirs("/home/p.toml").is_empty());
        assert!(dirs("profiles.toml").is_empty());
        assert!(dirs("conf/profiles.toml").is_empty());
    }

    #[test]
    fn dot_dot_cannot_walk_the_chown_out_of_home() {
        // Lexically these all "start with" /home/ann, but they name /etc, /
        // and /home: chowning their parents would hand system directories to
        // the invoking user.
        let home = Path::new("/home/ann");
        for path in [
            "/home/ann/../../etc/cpu-control/p.toml",
            "/home/ann/../shared/p.toml",
            "/home/ann/.config/../../bob/p.toml",
            "/home/ann/a/../p.toml",
            "../p.toml",
        ] {
            assert!(dirs_inside_home(Path::new(path), home).is_empty(), "{path}");
        }
    }

    #[test]
    fn is_root_matches_id() {
        let out = std::process::Command::new("id").arg("-u").output().unwrap();
        assert_eq!(is_root(), String::from_utf8_lossy(&out.stdout).trim() == "0");
    }
}
