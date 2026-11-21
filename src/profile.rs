//! Profiles: named, declarative CPU configurations. Built-in presets plus
//! user-defined ones stored in a TOML file.

use crate::cpuset::CpuSet;
use crate::fsutil::write_atomic;
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
    /// Defaults for every CPU that ends up online (including CPUs the kernel
    /// refuses to take offline).
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
#[serde(deny_unknown_fields)]
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
        write_atomic(path, &toml::to_string_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }
}

/// Look up a profile: user-defined ones shadow built-ins.
pub fn lookup(file: &ProfileFile, name: &str) -> Option<Profile> {
    file.profile.get(name).cloned().or_else(|| find_builtin(name))
}

/// Every profile on offer: the built-ins that are not shadowed, then the saved ones.
pub fn available(file: ProfileFile) -> Vec<(String, Profile)> {
    let mut list: Vec<(String, Profile)> =
        builtin().into_iter().filter(|(n, _)| !file.profile.contains_key(*n)).map(|(n, p)| (n.to_string(), p)).collect();
    list.extend(file.profile);
    list
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

    // A CPU without hotplug control stays online whatever the profile says, so
    // the defaults for "every online CPU" have to reach it as well.
    let mut tuned = target_online;
    for c in topo.cpus.iter().filter(|c| !c.hotpluggable) {
        tuned.insert(c.id);
    }
    let defaults = Tune {
        cpus: tuned,
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
    type Key = (Option<String>, Option<String>, Option<u64>, Option<u64>);
    let online = topo.online();
    let mut groups: BTreeMap<Key, CpuSet> = BTreeMap::new();
    for c in topo.cpus.iter().filter(|c| c.online) {
        groups
            .entry((c.governor.clone(), c.epp.clone(), c.scaling_min_khz, c.scaling_max_khz))
            .or_default()
            .insert(c.id);
    }
    let khz = |v: Option<u64>| v.map(|k| format!("{k}khz"));
    let mut p = Profile {
        description: Some("Snapshot of live state".into()),
        online: Some(online.to_string()),
        turbo: globals.no_turbo.map(|nt| !nt),
        ..Default::default()
    };
    // The biggest group becomes the default; the rest are explicit overrides.
    let mut all: Vec<_> = groups.into_iter().collect();
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

    fn parse(toml_text: &str) -> Profile {
        let mut f: ProfileFile = toml::from_str(toml_text).unwrap();
        f.profile.remove("x").unwrap()
    }

    fn cpus(s: &str) -> CpuSet {
        CpuSet::parse_list(s).unwrap()
    }

    #[test]
    fn builtins_are_named_uniquely_and_described() {
        let all = builtin();
        let names: std::collections::BTreeSet<_> = all.iter().map(|(n, _)| *n).collect();
        assert_eq!(names.len(), all.len());
        assert_eq!(all.len(), 11);
        assert!(all.iter().all(|(_, p)| p.description.is_some() && p.online.is_some()));
        assert!(find_builtin("quiet").is_some() && find_builtin("Quiet").is_none() && find_builtin("").is_none());
    }

    #[test]
    fn builtin_online_sets_on_the_hybrid_machine() {
        let (_f, topo) = fake_meteor_lake();
        for (name, online) in [
            ("all", "0-21"),
            ("no-pcores", "12-21"),
            ("no-ecores", "0-11,20-21"),
            ("no-lpe", "0-19"),
            ("pcores-only", "0-11"),
            ("ecores-only", "12-19"),
            ("lpe-only", "20-21"),
            ("no-smt", "0-1,3,6,8,10,12-21"),
            ("pcores-no-smt", "0-1,3,6,8,10"),
            ("max-perf", "0-1,3,6,8,10"),
            ("quiet", "12-21"),
        ] {
            let req = to_request(&topo, &find_builtin(name).unwrap()).unwrap();
            assert_eq!(req.online, cpus(online), "{name}");
            assert_eq!(req.offline, topo.all().difference(&cpus(online)), "{name}");
        }
        let max = to_request(&topo, &find_builtin("max-perf").unwrap()).unwrap();
        assert_eq!(max.turbo, Some(true));
        assert_eq!((max.tune[0].governor.as_deref(), max.tune[0].epp.as_deref()), (Some("performance"), Some("performance")));
        assert_eq!(to_request(&topo, &find_builtin("quiet").unwrap()).unwrap().turbo, Some(false));
    }

    #[test]
    fn a_profile_without_online_or_offline_leaves_hotplug_alone() {
        let (f, _) = fake_meteor_lake();
        f.offline(13);
        let topo = f.discover();
        let req = to_request(&topo, &parse("[profile.x]\ngovernor = \"performance\"\n")).unwrap();
        assert!(req.online.is_empty() && req.offline.is_empty());
        // The default still targets what is online right now.
        assert_eq!(req.tune.len(), 1);
        assert_eq!(req.tune[0].cpus, cpus("0-12,14-21"));
        assert_eq!(req.turbo, None);
    }

    #[test]
    fn offline_is_subtracted_after_online() {
        let (_f, topo) = fake_meteor_lake();
        let req = to_request(&topo, &parse("[profile.x]\nonline = \"p\"\noffline = \"smt\"\n")).unwrap();
        assert_eq!(req.online, cpus("0-1,3,6,8,10"));
        assert_eq!(req.offline, cpus("2,4-5,7,9,11-21"));
        // `offline` alone starts from the current online set.
        let req = to_request(&topo, &parse("[profile.x]\noffline = \"lpe\"\n")).unwrap();
        assert_eq!((req.online, req.offline), (cpus("0-19"), cpus("20-21")));
    }

    #[test]
    fn profiles_that_would_leave_nothing_online_are_refused() {
        let (_f, topo) = fake_meteor_lake();
        for text in [
            "[profile.x]\nonline = \"none\"\n",
            "[profile.x]\nonline = \"p,^p\"\n",
            "[profile.x]\nonline = \"e\"\noffline = \"all\"\n",
            "[profile.x]\noffline = \"all\"\n",
        ] {
            assert!(to_request(&topo, &parse(text)).is_err(), "{text}");
        }
    }

    #[test]
    fn defaults_reach_cpus_that_cannot_go_offline() {
        // `quiet` keeps only E and LP-E cores, but cpu0 has no hotplug control
        // and stays up: it must get the power-saving settings too.
        let (f, topo) = fake_meteor_lake();
        let req = to_request(&topo, &find_builtin("quiet").unwrap()).unwrap();
        assert_eq!(req.tune[0].cpus, cpus("0,12-21"));
        let plan = crate::plan::build_plan(&topo, &f.root(), &req).unwrap();
        assert!(plan.actions.iter().any(|a| a.path.ends_with("cpu0/cpufreq/scaling_governor") && a.value == "powersave"));
        assert!(!plan.warnings.iter().any(|w| w.contains("skipping")), "{:?}", plan.warnings);
    }

    #[test]
    fn tune_entries_follow_the_defaults_in_order() {
        let (_f, topo) = fake_meteor_lake();
        let p = parse(
            "[profile.x]\nonline = \"all\"\ngovernor = \"powersave\"\nmin_freq = \"800\"\nmax_freq = \"3ghz\"\n\
             [[profile.x.tune]]\ncpus = \"e\"\nmax_freq = \"2.4ghz\"\n\
             [[profile.x.tune]]\ncpus = \"lpe\"\nepp = \"power\"\ngovernor = \"performance\"\nmin_freq = \"400000khz\"\n",
        );
        let req = to_request(&topo, &p).unwrap();
        assert_eq!(req.tune.len(), 3);
        assert_eq!(
            req.tune[0],
            Tune { cpus: cpus("0-21"), governor: Some("powersave".into()), epp: None, min_khz: Some(800_000), max_khz: Some(3_000_000) }
        );
        assert_eq!(req.tune[1], Tune { cpus: cpus("12-19"), max_khz: Some(2_400_000), ..Default::default() });
        assert_eq!(
            req.tune[2],
            Tune { cpus: cpus("20-21"), governor: Some("performance".into()), epp: Some("power".into()), min_khz: Some(400_000), max_khz: None }
        );
    }

    #[test]
    fn bad_values_inside_a_profile_are_errors() {
        let (_f, topo) = fake_meteor_lake();
        for text in [
            "[profile.x]\nonline = \"nonsense\"\n",
            "[profile.x]\nonline = \"0-40\"\n",
            "[profile.x]\noffline = \"core:999\"\n",
            "[profile.x]\nmin_freq = \"fast\"\n",
            "[profile.x]\nmax_freq = \"inf\"\n",
            "[profile.x]\n[[profile.x.tune]]\ncpus = \"zzz\"\n",
            "[profile.x]\n[[profile.x.tune]]\ncpus = \"e\"\nmax_freq = \"-1\"\n",
        ] {
            assert!(to_request(&topo, &parse(text)).is_err(), "{text}");
        }
    }

    #[test]
    fn the_file_format_is_strict() {
        for text in [
            "[profiles.a]\nonline = \"p\"\n",                  // misspelt table
            "version = 2\n",                                   // unknown top-level key
            "[profile.a]\nturbo = \"yes\"\n",                  // wrong type
            "[profile.a]\n[[profile.a.tune]]\ngovernor = \"x\"\n", // tune without cpus
            "[profile.a]\n[[profile.a.tune]]\ncpus = \"e\"\nmax = \"2ghz\"\n", // unknown tune key
        ] {
            assert!(toml::from_str::<ProfileFile>(text).is_err(), "{text}");
        }
        assert!(toml::from_str::<ProfileFile>("").unwrap().profile.is_empty());
        assert!(toml::from_str::<ProfileFile>("[profile.a]\n").unwrap().profile.contains_key("a"));
    }

    #[test]
    fn load_and_save() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("conf/dir/profiles.toml");
        // No file yet is not an error.
        assert!(ProfileFile::load(&path).unwrap().profile.is_empty());

        let mut file = ProfileFile::default();
        file.profile.insert("mine".into(), find_builtin("max-perf").unwrap());
        file.profile.insert("odd name \"with\" quotes".into(), Profile::default());
        file.save(&path).unwrap();
        let back = ProfileFile::load(&path).unwrap();
        assert_eq!(back.profile, file.profile);
        // Nothing but the file itself is left in the directory.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        fs::write(&path, "[profile.a]\nonline = \n").unwrap();
        let err = format!("{:#}", ProfileFile::load(&path).unwrap_err());
        assert!(err.contains("parsing") && err.contains("profiles.toml"), "{err}");

        // A directory in the way is reported with the path, not swallowed.
        let err = format!("{:#}", ProfileFile::load(d.path()).unwrap_err());
        assert!(err.contains("reading"), "{err}");
        assert!(file.save(d.path()).is_err());
    }

    #[test]
    fn saved_profiles_shadow_builtins() {
        let mut file = ProfileFile::default();
        assert_eq!(lookup(&file, "quiet"), find_builtin("quiet"));
        assert_eq!(lookup(&file, "nope"), None);
        let mine = Profile { description: Some("mine".into()), ..Default::default() };
        file.profile.insert("quiet".into(), mine.clone());
        file.profile.insert("zz".into(), mine.clone());
        assert_eq!(lookup(&file, "quiet"), Some(mine.clone()));

        let list = available(file);
        let names: Vec<&str> = list.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names.len(), 12); // 11 built-ins, one shadowed, plus "zz"
        assert_eq!(names.iter().filter(|n| **n == "quiet").count(), 1);
        assert_eq!(list.iter().find(|(n, _)| n == "quiet").unwrap().1, mine);
        assert_eq!(names[..2], ["all", "no-pcores"]); // built-ins first, in their order
        assert_eq!(names[10..], ["quiet", "zz"]); // then the saved ones
    }

    #[test]
    fn snapshot_groups_cpus_by_their_settings() {
        let (f, _) = fake_meteor_lake();
        f.offline(21);
        f.write("cpu12/cpufreq/scaling_governor", "performance");
        f.write("cpu13/cpufreq/scaling_governor", "performance");
        let topo = f.discover();
        let snap = snapshot(&topo, &Globals { no_turbo: Some(true), ..Default::default() });
        assert_eq!(snap.online.as_deref(), Some("0-20"));
        assert_eq!(snap.offline, None);
        assert_eq!(snap.turbo, Some(false));
        // The biggest group (eight P threads at 4.5 GHz) is the default...
        assert_eq!(snap.governor.as_deref(), Some("powersave"));
        assert_eq!((snap.min_freq.as_deref(), snap.max_freq.as_deref()), (Some("400000khz"), Some("4500000khz")));
        // ...and every other combination is its own override.
        let overrides: BTreeMap<&str, &TuneEntry> = snap.tune.iter().map(|t| (t.cpus.as_str(), t)).collect();
        assert_eq!(overrides.keys().copied().collect::<Vec<_>>(), ["1-4", "12-13", "14-19", "20"]);
        assert_eq!(overrides["12-13"].governor.as_deref(), Some("performance"));
        assert_eq!(overrides["12-13"].max_freq.as_deref(), Some("3800000khz"));
        assert_eq!(overrides["20"].max_freq.as_deref(), Some("2500000khz"));
        // No turbo control: the snapshot leaves turbo out.
        assert_eq!(snapshot(&topo, &Globals::default()).turbo, None);
    }

    #[test]
    fn applying_a_snapshot_reproduces_the_state_it_was_taken_from() {
        let (f, _) = fake_meteor_lake();
        f.offline(21);
        f.write("cpu12/cpufreq/scaling_governor", "performance");
        f.write("cpu3/cpufreq/scaling_max_freq", "2000000");
        f.write("cpu3/cpufreq/scaling_min_freq", "800000");
        let before = f.discover();
        let snap = snapshot(&before, &Globals { no_turbo: Some(false), ..Default::default() });

        // Disturb everything, then apply the snapshot.
        f.online(21);
        f.offline(15);
        f.write("intel_pstate/no_turbo", "1");
        for id in [0, 3, 12, 19] {
            f.write(&format!("cpu{id}/cpufreq/scaling_governor"), "powersave");
            f.write(&format!("cpu{id}/cpufreq/scaling_max_freq"), "1000000");
            f.write(&format!("cpu{id}/cpufreq/scaling_min_freq"), "1000000");
        }
        let now = f.discover();
        let plan = crate::plan::build_plan(&now, &f.root(), &to_request(&now, &snap).unwrap()).unwrap();
        // cpu15 has to come up before its settings can be written.
        let up = plan.actions.iter().take_while(|a| a.step == crate::plan::Step::Online).count();
        assert!(crate::plan::apply(&crate::plan::Plan { actions: plan.actions[..up].to_vec(), ..Default::default() }).failed.is_empty());
        f.settle();
        let rest = crate::plan::Plan { actions: plan.actions[up..].to_vec(), ..Default::default() };
        let report = crate::plan::apply(&rest);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        f.settle();

        let after = f.discover();
        assert_eq!(after.online(), before.online());
        assert_eq!(f.read("intel_pstate/no_turbo"), "0");
        for (a, b) in after.cpus.iter().zip(&before.cpus).filter(|(a, _)| a.online) {
            assert_eq!(
                (&a.governor, &a.epp, a.scaling_min_khz, a.scaling_max_khz),
                (&b.governor, &b.epp, b.scaling_min_khz, b.scaling_max_khz),
                "cpu{}",
                a.id
            );
        }
    }

    #[test]
    fn describe_lists_only_what_is_set() {
        assert_eq!(describe(&Profile::default()), "");
        let p = Profile {
            online: Some("p".into()),
            offline: Some("smt".into()),
            turbo: Some(false),
            governor: Some("powersave".into()),
            epp: Some("power".into()),
            min_freq: Some("800".into()),
            max_freq: Some("2.4ghz".into()),
            tune: vec![TuneEntry::default(), TuneEntry::default()],
            ..Default::default()
        };
        assert_eq!(describe(&p), "online=p offline=smt turbo=off governor=powersave epp=power min=800 MHz max=2.40 GHz +2 tune rule(s)");
        // An unparsable frequency is left out rather than shown wrong.
        assert_eq!(describe(&Profile { max_freq: Some("warp 9".into()), turbo: Some(true), ..Default::default() }), "turbo=on");
    }
}
