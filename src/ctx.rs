//! Runtime context shared by the CLI and the TUI: where sysfs lives, where
//! config/cache files go, and how requests get executed.

use crate::plan::{self, Plan, Request};
use crate::topology::{Globals, Topology, TopologyCache};
use anyhow::Result;
use std::env;
use std::fs;
use std::os::unix::fs::chown;
use std::path::{Path, PathBuf};

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
        // Best effort: remember what online CPUs look like for later.
        for p in &self.cache_paths {
            if TopologyCache::save(p, &topo).is_ok() {
                break;
            }
        }
        Ok((topo, Globals::read(&self.root)))
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
    if let Ok(user) = env::var("SUDO_USER") {
        if user != "root" {
            if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
                for line in passwd.lines() {
                    let f: Vec<&str> = line.split(':').collect();
                    if f.len() >= 6 && f[0] == user {
                        return Some(PathBuf::from(f[5]));
                    }
                }
            }
        }
    }
    env::var_os("HOME").map(PathBuf::from)
}

/// When running under sudo, hand files we created back to the invoking user.
pub fn fix_ownership(path: &Path) {
    let (Some(uid), Some(gid)) = (
        env::var("SUDO_UID").ok().and_then(|s| s.parse::<u32>().ok()),
        env::var("SUDO_GID").ok().and_then(|s| s.parse::<u32>().ok()),
    ) else {
        return;
    };
    let _ = chown(path, Some(uid), Some(gid));
    // Directories we may have created on the way are only touched when they
    // live strictly inside the user's home; never anything outside it.
    let Some(home) = user_home() else { return };
    let mut cur = path.parent();
    while let Some(p) = cur {
        if p == home || !p.starts_with(&home) {
            break;
        }
        let _ = chown(p, Some(uid), Some(gid));
        cur = p.parent();
    }
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
