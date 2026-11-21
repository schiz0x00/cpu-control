//! End-to-end tests: run the real binary against a fake sysfs tree.
//!
//! Every invocation gets `--sysfs-root`, `--profiles` and a private state
//! directory, so nothing here can touch the machine running the tests.

#[allow(dead_code)]
#[path = "support/sysfs_tree.rs"]
mod sysfs_tree;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use sysfs_tree::{Fake, fake_sys};

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    #[track_caller]
    fn ok(self) -> Run {
        assert_eq!(self.code, 0, "expected success\nstdout:\n{}\nstderr:\n{}", self.out, self.err);
        self
    }

    #[track_caller]
    fn fails(self, code: i32, needle: &str) -> Run {
        assert_eq!(self.code, code, "stdout:\n{}\nstderr:\n{}", self.out, self.err);
        assert!(self.err.contains(needle), "stderr lacks {needle:?}:\n{}", self.err);
        self
    }
}

fn profiles(f: &Fake) -> PathBuf {
    f.dir.path().join("config/profiles.toml")
}

fn state(f: &Fake) -> PathBuf {
    f.dir.path().join("state")
}

/// The binary, aimed at the fake tree and cut off from the caller's environment.
fn command(f: &Fake) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_cpu-control"));
    c.arg("--sysfs-root").arg(f.root());
    c.env("CPU_CONTROL_STATE_DIR", state(f));
    c.env("NO_COLOR", "1");
    for var in ["SUDO_UID", "SUDO_GID", "SUDO_USER", "CPU_CONTROL_PROFILES"] {
        c.env_remove(var);
    }
    c.stdin(Stdio::null());
    c
}

fn finish(mut c: Command, args: &[&str]) -> Run {
    // With no subcommand the binary opens the GUI, and `tui` grabs the
    // terminal: neither may ever be started from a test.
    let sub = args.iter().find(|a| !a.starts_with('-')).copied();
    assert!(sub.is_some() || args.iter().any(|a| ["--help", "--version"].contains(a)), "refusing to launch the GUI: {args:?}");
    assert!(!matches!(sub, Some("gui" | "tui")), "refusing to launch an interactive UI");
    let o = c.args(args).output().unwrap();
    Run {
        code: o.status.code().expect("killed by a signal"),
        out: String::from_utf8(o.stdout).unwrap(),
        err: String::from_utf8(o.stderr).unwrap(),
    }
}

fn cli(f: &Fake, args: &[&str]) -> Run {
    let mut c = command(f);
    c.arg("--profiles").arg(profiles(f));
    finish(c, args)
}

/// Which CPUs the fake tree has online, in list form.
fn online(f: &Fake) -> String {
    cli(f, &["-n", "select", "online"]).ok().out.trim().to_string()
}

fn knob(f: &Fake, cpu: usize, name: &str) -> String {
    f.read(&format!("cpu{cpu}/cpufreq/{name}"))
}

/// A line of the preset listing, padded the way the tool prints it.
fn listing(name: &str, description: &str) -> String {
    format!("  {name:<14} {description}\n")
}

// ---------------------------------------------------------------- basics

#[test]
fn help_and_version() {
    let f = fake_sys();
    let help = cli(&f, &["--help"]).ok().out;
    for needle in ["Selectors", "--dry-run", "preset", "profile", "status"] {
        assert!(help.contains(needle), "help lacks {needle:?}");
    }
    assert!(!help.contains("--sysfs-root"), "the test hook stays hidden");
    assert_eq!(cli(&f, &["--version"]).ok().out.trim(), concat!("cpu-control ", env!("CARGO_PKG_VERSION")));
    assert!(cli(&f, &["set", "--help"]).ok().out.contains("--reset-freq"));
}

#[test]
fn bad_command_lines_exit_2_without_touching_anything() {
    let f = fake_sys();
    for args in [&["frobnicate"][..], &["off"], &["turbo", "maybe"], &["smt"], &["set", "e", "--min", "1ghz", "--reset-freq"], &["status", "--bogus"]] {
        let r = cli(&f, args);
        assert_eq!(r.code, 2, "{args:?}: {}", r.err);
        assert!(r.out.is_empty());
    }
    assert_eq!(online(&f), "0-21");
}

#[test]
fn an_unreadable_sysfs_root_is_a_clear_error() {
    let f = fake_sys();
    fs::remove_file(f.root().join("present")).unwrap();
    cli(&f, &["status"]).fails(2, "cannot read CPU list under");
    f.write("present", "");
    cli(&f, &["status"]).fails(2, "no CPUs found");
}

// ---------------------------------------------------------------- status / select

#[test]
fn status_lists_every_cpu_and_the_totals() {
    let f = fake_sys();
    let out = cli(&f, &["status"]).ok().out;
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("CPU  TYPE") && lines[0].ends_with("EPP"));
    assert_eq!(lines.len(), 1 + 22 + 4, "{out}");
    assert_eq!(lines[1].split_whitespace().collect::<Vec<_>>(), ["0", "P", "16", "T0", "#0", "fixed", "1.20", "GHz", "400", "MHz-4.50", "GHz", "powersave", "balance_performance"]);
    let row = |id: &str| lines.iter().find(|l| l.split_whitespace().next() == Some(id)).unwrap().split_whitespace().take(6).collect::<Vec<_>>();
    assert_eq!(row("5"), ["5", "P", "16", "T1", "#0", "online"]);
    assert_eq!(row("13"), ["13", "E", "1", "T0", "#12", "online"]);
    assert_eq!(row("21"), ["21", "LP-E", "33", "T0", "#20", "online"]);
    assert_eq!(lines[24], "P: 12/12 online  E: 8/8 online  LP-E: 2/2 online  |  total 22/22 online (0-21)");
    assert_eq!(lines[25], "smt=on  turbo=on  intel_pstate=active");
    assert_eq!(lines[26], "cpu0 cannot be taken offline (no hotplug support)");
    assert!(!out.contains('\x1b'), "no colour codes when not on a terminal");
    for alias in ["list", "ls"] {
        assert_eq!(cli(&f, &[alias]).ok().out, out);
    }
}

#[test]
fn status_flags_offline_cpus_and_unknown_topology() {
    let f = fake_sys();
    f.offline(12);
    let out = cli(&f, &["status"]).ok().out;
    let row: Vec<&str> = out.lines().find(|l| l.starts_with("12 ")).unwrap().split_whitespace().collect();
    assert_eq!(row[..6], ["12", "?", "?", "T0", "#12", "OFFLINE"], "type and core are unknown, not guessed");
    assert!(out.contains("total 21/22 online (0-11,13-21)"));
    assert!(out.contains("note: some offline CPUs have unknown topology"));

    // Seen online once, the facts stick.
    f.online(12);
    cli(&f, &["status"]).ok();
    f.offline(12);
    let out = cli(&f, &["status"]).ok().out;
    let row: Vec<&str> = out.lines().find(|l| l.starts_with("12 ")).unwrap().split_whitespace().collect();
    assert_eq!(row[..6], ["12", "E", "0", "T0", "#12", "OFFLINE"]);
    assert!(!out.contains("note:"));
}

#[test]
fn select_prints_what_a_selector_means() {
    let f = fake_sys();
    for (expr, want) in [("p,^smt", "0-1,3,6,8,10"), ("all,^p", "12-21"), ("l2:12", "12-15"), ("core:8", "1-2"), ("none", "-"), ("LPE", "20-21")] {
        assert_eq!(cli(&f, &["select", expr]).ok().out.trim(), want, "{expr}");
    }
    cli(&f, &["select", "bogus"]).fails(2, "unknown selector `bogus`");
    cli(&f, &["select", "40"]).fails(2, "cpu40 does not exist on this system (valid: 0-21)");
}

#[test]
fn an_absurd_range_is_rejected_at_once() {
    let f = fake_sys();
    let started = std::time::Instant::now();
    cli(&f, &["select", "0-99999999999"]).fails(2, "unknown selector");
    cli(&f, &["off", "5-18446744073709551615"]).fails(2, "unknown selector");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
}

// ---------------------------------------------------------------- hotplug

#[test]
fn off_and_on_switch_cpus() {
    let f = fake_sys();
    let r = cli(&f, &["off", "12-13"]).ok();
    assert_eq!(r.out, "  did: take cpu12 offline\n  did: take cpu13 offline\n2 change(s) applied, 0 failed\n");
    assert_eq!(r.err, "warning: online CPUs will be non-contiguous (0-11,14-21); some monitoring tools (e.g. btop 1.4.x) crash in that state\n");
    f.settle();
    assert_eq!(online(&f), "0-11,14-21");

    assert_eq!(cli(&f, &["off", "12-13"]).ok().out, "nothing to change\n");
    let r = cli(&f, &["on", "offline"]).ok();
    assert!(r.out.contains("did: bring cpu12 online") && r.out.contains("2 change(s) applied"));
    f.settle();
    assert_eq!(online(&f), "0-21");
    assert_eq!(cli(&f, &["on", "all"]).ok().out, "nothing to change\n");
}

#[test]
fn the_fixed_cpu_is_left_alone_with_a_warning() {
    let f = fake_sys();
    let r = cli(&f, &["off", "0"]).ok();
    assert_eq!(r.out, "nothing to change\n");
    assert!(r.err.contains("warning: cpu0 cannot be taken offline"));
    let r = cli(&f, &["off", "p"]).ok();
    assert!(r.out.contains("11 change(s) applied, 0 failed") && r.err.contains("cpu0 cannot be taken offline"));
    f.settle();
    assert_eq!(online(&f), "0,12-21");
}

#[test]
fn only_makes_exactly_the_selection_online() {
    let f = fake_sys();
    cli(&f, &["status"]).ok(); // records the topology while everything is up
    f.offline(1);
    f.offline(20);
    cli(&f, &["only", "p,^smt,lpe"]).ok();
    f.settle();
    assert_eq!(online(&f), "0-1,3,6,8,10,20-21");
    cli(&f, &["only", "none"]).fails(2, "matched no CPUs");
    cli(&f, &["only", "e,^e"]).fails(2, "matched no CPUs");
    assert_eq!(online(&f), "0-1,3,6,8,10,20-21");
}

#[test]
fn smt_selectors_survive_the_first_thread_going_offline() {
    // With cpu1 offline the kernel reports cpu2 as a core of its own. The tool
    // must still know cpu2 is the second thread, or `p,^smt` keeps both.
    let f = fake_sys();
    cli(&f, &["status"]).ok(); // records the topology
    cli(&f, &["off", "1"]).ok();
    f.settle();
    assert_eq!(f.read("cpu2/topology/thread_siblings_list"), "2");
    assert_eq!(cli(&f, &["select", "smt"]).ok().out.trim(), "2,4-5,7,9,11");

    cli(&f, &["preset", "pcores-no-smt"]).ok();
    f.settle();
    assert_eq!(online(&f), "0-1,3,6,8,10");
    // Applying it again changes nothing: the result is stable.
    assert_eq!(cli(&f, &["preset", "pcores-no-smt"]).ok().out, "nothing to change\n");
}

#[test]
fn a_dry_run_changes_nothing_on_disk() {
    let f = fake_sys();
    for args in [&["-n", "off", "e"][..], &["off", "e", "--dry-run"], &["-n", "preset", "quiet"], &["-n", "set", "all", "--governor", "performance", "--max", "2ghz"], &["-n", "turbo", "off"], &["-n", "smt", "off"]] {
        let r = cli(&f, args).ok();
        assert!(r.out.contains("  would: ") && r.out.ends_with("(dry run: nothing was written)\n"), "{args:?}: {}", r.out);
        assert!(!r.out.contains("did:") && !r.out.contains("applied"));
    }
    assert_eq!(online(&f), "0-21");
    assert_eq!((f.read("intel_pstate/no_turbo"), f.read("smt/control")), ("0".to_string(), "on".to_string()));
    assert_eq!((knob(&f, 0, "scaling_governor"), knob(&f, 12, "scaling_max_freq")), ("powersave".to_string(), "3800000".to_string()));
    assert!(!state(&f).exists(), "a dry run must not even write the topology cache");
    assert!(!profiles(&f).exists());
}

#[test]
fn a_write_that_fails_gives_exit_code_1_and_the_rest_still_runs() {
    let f = fake_sys();
    // A directory where the knob should be: unwritable even for root.
    let gov = f.root().join("cpu13/cpufreq/scaling_governor");
    fs::remove_file(&gov).unwrap();
    fs::create_dir(&gov).unwrap();
    let r = cli(&f, &["set", "12-14", "--governor", "performance"]);
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("failed: cpu13 governor -> performance: "), "{}", r.err);
    assert!(r.out.ends_with("2 change(s) applied, 1 failed\n"), "{}", r.out);
    assert_eq!((knob(&f, 12, "scaling_governor"), knob(&f, 14, "scaling_governor")), ("performance".to_string(), "performance".to_string()));
}

#[test]
fn permission_denied_points_at_sudo() {
    use std::os::unix::fs::PermissionsExt;
    let f = fake_sys();
    let file = f.root().join("cpu13/online");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(&file).is_ok() {
        return; // running as root: file modes do not apply
    }
    let r = cli(&f, &["off", "13"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("failed: take cpu13 offline: Permission denied"), "{}", r.err);
    assert!(r.err.contains("hint: permission denied: run with sudo"), "{}", r.err);
    assert_eq!(f.read("cpu13/online"), "1");
}

// ---------------------------------------------------------------- tuning

#[test]
fn set_writes_governor_epp_and_limits() {
    let f = fake_sys();
    let r = cli(&f, &["set", "e", "-g", "performance", "-e", "performance", "--min", "800", "--max", "2.4ghz"]).ok();
    assert!(r.out.ends_with("32 change(s) applied, 0 failed\n"), "{}", r.out);
    for id in 12..20 {
        assert_eq!(
            [knob(&f, id, "scaling_governor"), knob(&f, id, "energy_performance_preference"), knob(&f, id, "scaling_min_freq"), knob(&f, id, "scaling_max_freq")],
            ["performance", "performance", "800000", "2400000"],
            "cpu{id}"
        );
    }
    assert_eq!((knob(&f, 11, "scaling_governor"), knob(&f, 20, "scaling_max_freq")), ("powersave".to_string(), "2500000".to_string()));
}

#[test]
fn reset_freq_restores_each_cpus_own_hardware_range() {
    let f = fake_sys();
    cli(&f, &["set", "all", "--min", "1ghz", "--max", "1.5ghz"]).ok();
    assert_eq!(knob(&f, 1, "scaling_max_freq"), "1500000");
    cli(&f, &["set", "all", "--reset-freq"]).ok();
    for (id, max) in [(0, "4500000"), (1, "4800000"), (12, "3800000"), (21, "2500000")] {
        assert_eq!((knob(&f, id, "scaling_min_freq"), knob(&f, id, "scaling_max_freq")), ("400000".to_string(), max.to_string()), "cpu{id}");
    }
    // It combines with a governor change.
    cli(&f, &["set", "20", "--reset-freq", "--governor", "performance"]).ok();
    assert_eq!(knob(&f, 20, "scaling_governor"), "performance");
}

#[test]
fn set_refuses_requests_that_make_no_sense() {
    let f = fake_sys();
    cli(&f, &["set", "e"]).fails(2, "nothing to set");
    cli(&f, &["set", "none", "--governor", "performance"]).fails(2, "matched no CPUs");
    cli(&f, &["set", "e", "--max", "fast"]).fails(2, "invalid frequency `fast`");
    cli(&f, &["set", "e", "--max", "inf"]).fails(2, "frequency must be positive");
    cli(&f, &["set", "e", "--max", "1e30"]).fails(2, "out of range");
    cli(&f, &["set", "e", "--min", "3ghz", "--max", "2ghz"]).fails(2, "min frequency (3.00 GHz) is above max frequency (2.00 GHz)");
    cli(&f, &["set", "e", "--min", "4ghz"]).fails(2, "exceeds current max");
    cli(&f, &["set", "e", "--max", "100"]).fails(2, "below current min");
    assert_eq!((knob(&f, 12, "scaling_min_freq"), knob(&f, 12, "scaling_max_freq")), ("400000".to_string(), "3800000".to_string()));
}

#[test]
fn governor_and_epp_values_cannot_smuggle_content_into_a_file() {
    let f = fake_sys();
    let payload = "x\nALL ALL=(ALL) NOPASSWD: ALL";
    cli(&f, &["set", "1", "--governor", payload]).fails(2, "invalid governor");
    cli(&f, &["set", "1", "--epp", "../../x"]).fails(2, "invalid energy preference");
    cli(&f, &["set", "1", "--governor", ""]).fails(2, "invalid governor");
    assert_eq!(knob(&f, 1, "scaling_governor"), "powersave");

    // A knob that does not exist is reported, never created.
    fs::remove_file(f.root().join("cpu1/cpufreq/energy_performance_preference")).unwrap();
    let r = cli(&f, &["set", "1", "--epp", "power"]);
    assert_eq!(r.code, 1);
    assert!(!f.exists("cpu1/cpufreq/energy_performance_preference"));
}

#[test]
fn tuning_skips_cpus_that_are_offline() {
    let f = fake_sys();
    cli(&f, &["status"]).ok();
    f.offline(13);
    let r = cli(&f, &["set", "12-14", "--governor", "performance"]).ok();
    assert!(r.err.contains("warning: cpu13 will be offline; skipping its frequency/governor settings"));
    assert!(r.out.ends_with("2 change(s) applied, 0 failed\n"));
}

#[test]
fn turbo_and_smt_switches() {
    let f = fake_sys();
    assert_eq!(cli(&f, &["turbo", "off"]).ok().out, "  did: turbo boost disabled\n1 change(s) applied, 0 failed\n");
    assert_eq!(f.read("intel_pstate/no_turbo"), "1");
    assert!(cli(&f, &["status"]).ok().out.contains("turbo=off"));
    cli(&f, &["turbo", "on"]).ok();
    assert_eq!(f.read("intel_pstate/no_turbo"), "0");

    cli(&f, &["smt", "off"]).ok();
    assert_eq!(f.read("smt/control"), "off");
    cli(&f, &["smt", "on"]).ok();
    assert_eq!(f.read("smt/control"), "on");
}

#[test]
fn turbo_falls_back_to_the_boost_flag_or_says_there_is_none() {
    let f = fake_sys();
    fs::remove_dir_all(f.root().join("intel_pstate")).unwrap();
    let r = cli(&f, &["turbo", "off"]).ok();
    assert!(r.err.contains("warning: no turbo/boost control found") && r.out == "nothing to change\n");
    assert!(!f.exists("intel_pstate"), "nothing was created");

    f.write("cpufreq/boost", "1");
    assert!(cli(&f, &["status"]).ok().out.contains("turbo=on"));
    assert!(cli(&f, &["turbo", "off"]).ok().out.contains("did: boost disabled"));
    assert_eq!(f.read("cpufreq/boost"), "0");
    assert!(cli(&f, &["status"]).ok().out.contains("turbo=off"));
}

// ---------------------------------------------------------------- presets and profiles

#[test]
fn preset_without_a_name_lists_them() {
    let f = fake_sys();
    let out = cli(&f, &["preset"]).ok().out;
    assert!(out.starts_with("Built-in presets:\n"));
    for name in ["all", "no-pcores", "no-ecores", "no-lpe", "pcores-only", "ecores-only", "lpe-only", "no-smt", "pcores-no-smt", "max-perf", "quiet"] {
        assert!(out.lines().any(|l| l.split_whitespace().next() == Some(name)), "{name} missing:\n{out}");
    }
    assert!(!out.contains("Saved profiles"));
    assert_eq!(cli(&f, &["profile", "list"]).ok().out, out);
}

#[test]
fn applying_builtin_presets() {
    let f = fake_sys();
    cli(&f, &["preset", "max-perf"]).ok();
    f.settle();
    assert_eq!(online(&f), "0-1,3,6,8,10");
    for id in [0, 1, 3, 6, 8, 10] {
        assert_eq!((knob(&f, id, "scaling_governor"), knob(&f, id, "energy_performance_preference")), ("performance".to_string(), "performance".to_string()));
    }
    assert_eq!(f.read("intel_pstate/no_turbo"), "0");

    // From there to the opposite set: E and LP-E cores up, P-cores down.
    let r = cli(&f, &["preset", "no-pcores"]).ok();
    assert!(r.out.contains("did: bring cpu12 online") && r.out.contains("did: take cpu1 offline"));
    assert!(r.out.find("bring cpu21 online").unwrap() < r.out.find("take cpu1 offline").unwrap(), "up before down");
    f.settle();
    assert_eq!(online(&f), "0,12-21");

    assert!(cli(&f, &["preset", "all"]).ok().out.ends_with("11 change(s) applied, 0 failed\n"));
    f.settle();
    assert_eq!(online(&f), "0-21");
    cli(&f, &["preset", "nope"]).fails(2, "no preset or profile named `nope`");
}

#[test]
fn quiet_also_tames_the_cpu_that_cannot_go_offline() {
    let f = fake_sys();
    cli(&f, &["preset", "quiet"]).ok();
    f.settle();
    assert_eq!(online(&f), "0,12-21");
    assert_eq!(f.read("intel_pstate/no_turbo"), "1");
    for id in [0, 12, 19, 20, 21] {
        assert_eq!((knob(&f, id, "scaling_governor"), knob(&f, id, "energy_performance_preference")), ("powersave".to_string(), "power".to_string()), "cpu{id}");
    }
}

#[test]
fn profile_save_show_apply_delete() {
    let f = fake_sys();
    cli(&f, &["off", "20-21"]).ok();
    f.settle();
    cli(&f, &["set", "12-15", "--governor", "performance", "--max", "2ghz"]).ok();
    cli(&f, &["turbo", "off"]).ok();

    let r = cli(&f, &["profile", "save", "mine", "-d", "my desc"]).ok();
    assert!(r.out.starts_with(&format!("saved profile `mine` to {}\n", profiles(&f).display())), "{}", r.out);
    assert!(r.out.contains("online=0-19 turbo=off governor=powersave"));
    assert_eq!(fs::read_dir(profiles(&f).parent().unwrap()).unwrap().count(), 1, "no temp file left behind");

    let list = cli(&f, &["preset"]).ok().out;
    assert!(list.contains(&format!("\nSaved profiles ({}):\n{}", profiles(&f).display(), listing("mine", "my desc"))), "{list}");

    let shown = cli(&f, &["profile", "show", "mine"]).ok().out;
    assert!(shown.starts_with("[profile.mine]\n") && shown.contains("description = \"my desc\"") && shown.contains("online = \"0-19\""), "{shown}");
    assert!(shown.contains("[[profile.mine.tune]]\ncpus = \"12-15\"\ngovernor = \"performance\""), "{shown}");
    assert!(cli(&f, &["profile", "show", "quiet"]).ok().out.contains("online = \"e,lpe\""));
    cli(&f, &["profile", "show", "nope"]).fails(2, "no profile named `nope`");

    // Wreck the state, then get it back from the profile.
    cli(&f, &["preset", "all"]).ok();
    f.settle();
    cli(&f, &["set", "all", "--governor", "powersave", "--reset-freq"]).ok();
    cli(&f, &["turbo", "on"]).ok();
    cli(&f, &["profile", "apply", "mine"]).ok();
    f.settle();
    assert_eq!(online(&f), "0-19");
    assert_eq!(f.read("intel_pstate/no_turbo"), "1");
    assert_eq!((knob(&f, 12, "scaling_governor"), knob(&f, 12, "scaling_max_freq")), ("performance".to_string(), "2000000".to_string()));
    assert_eq!((knob(&f, 16, "scaling_governor"), knob(&f, 16, "scaling_max_freq")), ("powersave".to_string(), "3800000".to_string()));
    assert_eq!((knob(&f, 3, "scaling_governor"), knob(&f, 3, "scaling_max_freq")), ("powersave".to_string(), "4800000".to_string()));
    // `preset` finds saved profiles too; a second application moves no CPU.
    let again = cli(&f, &["preset", "mine"]).ok().out;
    assert!(!again.contains("online") && !again.contains("offline"), "{again}");

    assert_eq!(cli(&f, &["profile", "delete", "mine"]).ok().out, "deleted profile `mine`\n");
    cli(&f, &["profile", "delete", "mine"]).fails(2, "no saved profile named `mine`");
    cli(&f, &["profile", "delete", "quiet"]).fails(2, "no saved profile named `quiet`");
    cli(&f, &["preset", "mine"]).fails(2, "no preset or profile named `mine`");
    assert!(!cli(&f, &["preset"]).ok().out.contains("Saved profiles"));
}

#[test]
fn a_saved_profile_shadows_the_builtin_of_the_same_name() {
    let f = fake_sys();
    cli(&f, &["profile", "save", "quiet"]).ok();
    let list = cli(&f, &["preset"]).ok().out;
    assert_eq!(list.lines().filter(|l| l.split_whitespace().next() == Some("quiet")).count(), 1, "{list}");
    assert!(list.contains(&listing("quiet", "Snapshot of live state")), "{list}");
    // It now means "the state I saved" (everything online), not the built-in.
    assert!(!cli(&f, &["-n", "preset", "quiet"]).ok().out.contains("offline"));
    cli(&f, &["profile", "delete", "quiet"]).ok();
    assert!(cli(&f, &["-n", "preset", "quiet"]).ok().out.contains("would: take cpu1 offline"));
}

#[test]
fn profile_changes_respect_dry_run() {
    let f = fake_sys();
    let r = cli(&f, &["-n", "profile", "save", "x"]).ok();
    assert!(r.out.starts_with("(dry run) would save profile `x`"));
    assert!(!profiles(&f).exists());

    cli(&f, &["profile", "save", "x"]).ok();
    let before = fs::read_to_string(profiles(&f)).unwrap();
    assert_eq!(cli(&f, &["-n", "profile", "delete", "x"]).ok().out, "(dry run) would delete profile `x`\n");
    assert_eq!(fs::read_to_string(profiles(&f)).unwrap(), before);
    cli(&f, &["-n", "profile", "delete", "nope"]).fails(2, "no saved profile named `nope`");
}

#[test]
fn a_hand_written_profile_with_overrides() {
    let f = fake_sys();
    fs::create_dir_all(profiles(&f).parent().unwrap()).unwrap();
    fs::write(
        profiles(&f),
        r#"
[profile.gaming]
description = "P-cores w/o SMT, E-cores capped"
online = "p,^smt,e"
turbo = true
governor = "performance"
epp = "performance"

[[profile.gaming.tune]]
cpus = "e"
max_freq = "2.4ghz"
"#,
    )
    .unwrap();
    assert!(cli(&f, &["preset"]).ok().out.contains(&listing("gaming", "P-cores w/o SMT, E-cores capped")));
    cli(&f, &["preset", "gaming"]).ok();
    f.settle();
    assert_eq!(online(&f), "0-1,3,6,8,10,12-19");
    assert_eq!((knob(&f, 3, "scaling_governor"), knob(&f, 3, "scaling_max_freq")), ("performance".to_string(), "4800000".to_string()));
    assert_eq!((knob(&f, 15, "scaling_governor"), knob(&f, 15, "scaling_max_freq")), ("performance".to_string(), "2400000".to_string()));
}

#[test]
fn a_broken_profiles_file_stops_profile_commands_but_not_the_rest() {
    let f = fake_sys();
    fs::create_dir_all(profiles(&f).parent().unwrap()).unwrap();
    for bad in ["[profile.x\n", "[profile.x]\nonlin = \"p\"\n", "[profiles.x]\nonline = \"p\"\n"] {
        fs::write(profiles(&f), bad).unwrap();
        for args in [&["preset"][..], &["preset", "quiet"], &["profile", "save", "y"], &["profile", "delete", "x"], &["profile", "show", "x"]] {
            cli(&f, args).fails(2, "profiles.toml");
        }
        assert_eq!(fs::read_to_string(profiles(&f)).unwrap(), bad, "a file we cannot parse is never overwritten");
    }
    cli(&f, &["status"]).ok();
    cli(&f, &["off", "21"]).ok();

    // A profile that parses but does not fit the machine names itself.
    fs::write(profiles(&f), "[profile.x]\nonline = \"30-40\"\n").unwrap();
    cli(&f, &["preset", "x"]).fails(2, "preset `x`");
}

#[test]
fn the_profiles_path_can_come_from_the_environment() {
    let f = fake_sys();
    let path = f.dir.path().join("elsewhere/p.toml");
    let mut c = command(&f);
    c.env("CPU_CONTROL_PROFILES", &path);
    finish(c, &["profile", "save", "env"]).ok();
    assert!(fs::read_to_string(&path).unwrap().starts_with("[profile.env]"));
    // The flag wins over the variable.
    let mut c = command(&f);
    c.env("CPU_CONTROL_PROFILES", &path).arg("--profiles").arg(profiles(&f));
    assert!(!finish(c, &["preset"]).ok().out.contains("Saved profiles"));
}

#[test]
fn saving_through_a_symlinked_profiles_file_keeps_the_link() {
    let f = fake_sys();
    let real = f.dir.path().join("dotfiles/profiles.toml");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "").unwrap();
    fs::create_dir_all(profiles(&f).parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, profiles(&f)).unwrap();
    cli(&f, &["profile", "save", "linked"]).ok();
    assert!(fs::symlink_metadata(profiles(&f)).unwrap().file_type().is_symlink());
    assert!(fs::read_to_string(&real).unwrap().contains("[profile.linked]"));
}

// ---------------------------------------------------------------- cache

#[test]
fn the_topology_cache_is_written_once_and_then_left_alone() {
    use std::os::unix::fs::MetadataExt;
    let f = fake_sys();
    let cache = state(&f).join("topology.toml");
    cli(&f, &["status"]).ok();
    let text = fs::read_to_string(&cache).unwrap();
    assert!(text.starts_with("present = \"0-21\"\n") && text.contains("[cpu.21]") && text.contains("kind = \"LpE\""), "{text}");
    let inode = fs::metadata(&cache).unwrap().ino();
    cli(&f, &["status"]).ok();
    cli(&f, &["select", "p"]).ok();
    assert_eq!(fs::metadata(&cache).unwrap().ino(), inode, "unchanged facts are not rewritten");
    assert_eq!(fs::read_dir(state(&f)).unwrap().count(), 1);
}

#[test]
fn a_corrupt_cache_is_ignored_and_replaced() {
    let f = fake_sys();
    fs::create_dir_all(state(&f)).unwrap();
    fs::write(state(&f).join("topology.toml"), "present = \"0-21\"\n[cpu.0\n").unwrap();
    cli(&f, &["status"]).ok();
    assert!(fs::read_to_string(state(&f).join("topology.toml")).unwrap().contains("[cpu.21]"));
}
