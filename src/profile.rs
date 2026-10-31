//! Profiles: named, declarative CPU configurations. Built-in presets plus
//! user-defined ones stored in a TOML file.

use crate::cpuset::CpuSet;
use crate::plan::{Request, Tune, fmt_khz, parse_freq_khz};
use crate::selector;
use crate::topology::{Globals, Topology};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TuneEntry {
    /// Selector expression for the CPUs this entry applies to.
    pub cpus: String,
    pub governor: Option<String>,
    pub epp: Option<String>,
    pub min_freq: Option<String>,
    pub max_freq: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub description: Option<String>,
    /// Selector: *exactly* these CPUs end up online (others are offlined).
    /// Omit to leave the online set untouched.
    pub online: Option<String>,
    /// Extra selector of CPUs to force offline (applied after `online`).
    pub offline: Option<String>,
    pub turbo: Option<bool>,
    /// Defaults for every CPU that ends up online.
    pub governor: Option<String>,
    pub epp: Option<String>,
    pub min_freq: Option<String>,
    pub max_freq: Option<String>,
    /// Finer-grained overrides, applied after the defaults above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tune: Vec<TuneEntry>,
}

pub fn builtin() -> Vec<(&'static str, Profile)> {
    let p = |desc: &str, online: &str| Profile {
        description: Some(desc.into()),
        online: Some(online.into()),
        ..Default::default()
    };
    vec![
        ("all", p("Every core and thread online", "all")),
        ("no-pcores", p("Disable all P-cores (keep E and LP-E cores)", "all,^p")),
        ("no-ecores", p("Disable all E-cores (keep P and LP-E cores)", "all,^e")),
        ("no-lpe", p("Disable the low-power E-cores", "all,^lpe")),
        ("pcores-only", p("Only P-cores (all their threads)", "p")),
        ("ecores-only", p("Only the E-cores", "e")),
        ("lpe-only", p("Only the low-power E-cores", "lpe")),
        ("no-smt", p("One thread per core (disable hyper-threading)", "all,^smt")),
        ("pcores-no-smt", p("P-cores only, one thread each", "p,^smt")),
        (
            "max-perf",
            Profile {
                description: Some("P-cores without SMT, performance governor, turbo on".into()),
                online: Some("p,^smt".into()),
                turbo: Some(true),
                governor: Some("performance".into()),
                epp: Some("performance".into()),
                ..Default::default()
            },
        ),
        (
            "quiet",
            Profile {
                description: Some("E and LP-E cores only, turbo off, power-saving bias".into()),
                online: Some("e,lpe".into()),
                turbo: Some(false),
                governor: Some("powersave".into()),
                epp: Some("power".into()),
                ..Default::default()
            },
        ),
    ]
}

pub fn find_builtin(name: &str) -> Option<Profile> {
    builtin().into_iter().find(|(n, _)| *n == name).map(|(_, p)| p)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProfileFile {
    #[serde(default)]
    pub profile: BTreeMap<String, Profile>,
}

impl ProfileFile {
    pub fn load(path: &Path) -> Result<ProfileFile> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProfileFile::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, toml::to_string_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }
}

/// Look up a profile: user-defined ones shadow built-ins.
pub fn lookup(file: &ProfileFile, name: &str) -> Option<Profile> {
    file.profile.get(name).cloned().or_else(|| find_builtin(name))
}

/// Resolve a profile against the current topology into a concrete [`Request`].
pub fn to_request(topo: &Topology, p: &Profile) -> Result<Request> {
    let mut req = Request { turbo: p.turbo, ..Default::default() };

    let mut target_online = topo.online();
    if let Some(expr) = &p.online {
        let set = selector::eval(topo, expr)?;
        if set.is_empty() {
            bail!("`online = \"{expr}\"` selects no CPUs on this system");
        }
        target_online = set;
    }
    if let Some(expr) = &p.offline {
        target_online = target_online.difference(&selector::eval(topo, expr)?);
    }
    if target_online.is_empty() {
        bail!("profile would leave no CPUs online");
    }
    if p.online.is_some() || p.offline.is_some() {
        req.online = target_online.clone();
        req.offline = topo.all().difference(&target_online);
    }

    let defaults = Tune {
        cpus: target_online,
        governor: p.governor.clone(),
        epp: p.epp.clone(),
        min_khz: p.min_freq.as_deref().map(parse_freq_khz).transpose()?,
        max_khz: p.max_freq.as_deref().map(parse_freq_khz).transpose()?,
    };
    if !defaults.is_noop() {
        req.tune.push(defaults);
    }
    for t in &p.tune {
        req.tune.push(Tune {
            cpus: selector::eval(topo, &t.cpus)?,
            governor: t.governor.clone(),
            epp: t.epp.clone(),
            min_khz: t.min_freq.as_deref().map(parse_freq_khz).transpose()?,
            max_khz: t.max_freq.as_deref().map(parse_freq_khz).transpose()?,
        });
    }
    Ok(req)
}

/// Capture the live state as a profile (online set, grouped per-CPU tuning, turbo).
pub fn snapshot(topo: &Topology, globals: &Globals) -> Profile {
    let online = topo.online();
    let mut groups: BTreeMap<(Option<String>, Option<String>, Option<u64>, Option<u64>), CpuSet> = BTreeMap::new();
    for c in topo.cpus.iter().filter(|c| c.online) {
        groups
            .entry((c.governor.clone(), c.epp.clone(), c.scaling_min_khz, c.scaling_max_khz))
            .or_default()
            .insert(c.id);
    }
    let khz = |v: Option<u64>| v.map(|k| format!("{k}khz"));
    let mut it = groups.into_iter();
    let mut p = Profile {
        description: Some("Snapshot of live state".into()),
        online: Some(online.to_string()),
        turbo: globals.no_turbo.map(|nt| !nt),
        ..Default::default()
    };
    // The biggest group becomes the default; the rest are explicit overrides.
    let mut all: Vec<_> = it.by_ref().collect();
    all.sort_by_key(|(_, s)| std::cmp::Reverse(s.len()));
    for (i, ((gov, epp, min, max), set)) in all.into_iter().enumerate() {
        if i == 0 {
            p.governor = gov;
            p.epp = epp;
            p.min_freq = khz(min);
            p.max_freq = khz(max);
        } else {
            p.tune.push(TuneEntry {
                cpus: set.to_string(),
                governor: gov,
                epp,
                min_freq: khz(min),
                max_freq: khz(max),
            });
        }
    }
    p
}

pub fn describe(p: &Profile) -> String {
    let mut parts = Vec::new();
    if let Some(o) = &p.online {
        parts.push(format!("online={o}"));
    }
    if let Some(o) = &p.offline {
        parts.push(format!("offline={o}"));
    }
    if let Some(t) = p.turbo {
        parts.push(format!("turbo={}", if t { "on" } else { "off" }));
    }
    if let Some(g) = &p.governor {
        parts.push(format!("governor={g}"));
    }
    if let Some(e) = &p.epp {
        parts.push(format!("epp={e}"));
    }
    if let Some(f) = p.min_freq.as_deref().and_then(|s| parse_freq_khz(s).ok()) {
        parts.push(format!("min={}", fmt_khz(f)));
    }
    if let Some(f) = p.max_freq.as_deref().and_then(|s| parse_freq_khz(s).ok()) {
        parts.push(format!("max={}", fmt_khz(f)));
    }
    if !p.tune.is_empty() {
        parts.push(format!("+{} tune rule(s)", p.tune.len()));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::fake_meteor_lake;

    #[test]
    fn no_pcores_preset() {
        let (_f, topo) = fake_meteor_lake();
        let req = to_request(&topo, &find_builtin("no-pcores").unwrap()).unwrap();
        assert_eq!(req.online.to_string(), "12-21");
        assert_eq!(req.offline.to_string(), "0-11");
    }

    #[test]
    fn every_builtin_resolves_on_hybrid() {
        let (_f, topo) = fake_meteor_lake();
        for (name, p) in builtin() {
            to_request(&topo, &p).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn toml_roundtrip_and_snapshot() {
        let (_f, topo) = fake_meteor_lake();
        let snap = snapshot(&topo, &Globals { no_turbo: Some(false), ..Default::default() });
        assert_eq!(snap.online.as_deref(), Some("0-21"));
        assert_eq!(snap.turbo, Some(true));
        // Uniform machine -> a single default group, no overrides.
        assert!(snap.tune.is_empty() || snap.tune.len() <= 4);
        let mut f = ProfileFile::default();
        f.profile.insert("x".into(), snap.clone());
        let text = toml::to_string_pretty(&f).unwrap();
        let back: ProfileFile = toml::from_str(&text).unwrap();
        assert_eq!(back.profile["x"], snap);
    }

    #[test]
    fn empty_selection_is_an_error() {
        let (_f, mut topo) = fake_meteor_lake();
        topo.cpus.retain(|c| c.kind != crate::topology::CoreKind::LpE);
        let p = find_builtin("lpe-only").unwrap();
        assert!(to_request(&topo, &p).is_err());
    }

    #[test]
    fn unknown_field_rejected() {
        let r: Result<ProfileFile, _> = toml::from_str("[profile.a]\nonlin = \"p\"\n");
        assert!(r.is_err());
    }
}
