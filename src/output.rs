//! Human-readable rendering for the CLI.

use crate::ctx::Outcome;
use crate::plan::fmt_khz;
use crate::topology::{CoreKind, Globals, Topology};
use std::io::IsTerminal;

struct Style(bool);
impl Style {
    fn new() -> Style {
        Style(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
}

pub fn print_status(topo: &Topology, g: &Globals) {
    let st = Style::new();
    println!(
        "{:<4} {:<5} {:>4} {:<3} {:<9} {:<7} {:>9} {:>17}  {:<11} {}",
        "CPU", "TYPE", "CORE", "THR", "L2", "STATE", "CUR", "MIN-MAX", "GOVERNOR", "EPP"
    );
    for c in &topo.cpus {
        let thr = c.siblings.iter().position(|s| s == c.id).map(|i| format!("T{i}")).unwrap_or_else(|| "?".into());
        let core = if c.topology_known { c.core_id.to_string() } else { "?".into() };
        let l2 = c.l2_group.iter().next().map(|f| format!("#{f}")).unwrap_or_default();
        let state = match (c.online, c.hotpluggable) {
            (true, true) => st.paint("32", "online "),
            (true, false) => st.paint("33", "fixed  "),
            (false, _) => st.paint("31", "OFFLINE"),
        };
        let khz = |v: Option<u64>| v.map(fmt_khz).unwrap_or_else(|| "-".into());
        let range = match (c.scaling_min_khz, c.scaling_max_khz) {
            (Some(a), Some(b)) => format!("{}-{}", fmt_khz(a), fmt_khz(b)),
            _ => "-".into(),
        };
        let line = format!(
            "{:<4} {:<5} {:>4} {:<3} {:<9} {} {:>9} {:>17}  {:<11} {}",
            c.id,
            c.kind.to_string(),
            core,
            thr,
            l2,
            state,
            khz(c.cur_khz),
            range,
            c.governor.as_deref().unwrap_or("-"),
            c.epp.as_deref().unwrap_or("-"),
        );
        println!("{}", if c.online { line } else { st.paint("2", &line) });
    }

    println!();
    let mut kinds = Vec::new();
    for k in [CoreKind::P, CoreKind::E, CoreKind::LpE] {
        let all = topo.of_kind(k);
        if all.is_empty() {
            continue;
        }
        let on = all.intersection(&topo.online());
        kinds.push(format!("{k}: {}/{} online", on.len(), all.len()));
    }
    println!("{}  |  total {}/{} online ({})", kinds.join("  "), topo.online().len(), topo.cpus.len(), topo.online());
    let mut extra = Vec::new();
    if let Some(s) = &g.smt_control {
        extra.push(format!("smt={s}"));
    }
    if let Some(nt) = g.no_turbo {
        extra.push(format!("turbo={}", if nt { "off" } else { "on" }));
    }
    if let Some(s) = &g.pstate_status {
        extra.push(format!("intel_pstate={s}"));
    }
    if !extra.is_empty() {
        println!("{}", extra.join("  "));
    }
    if topo.cpus.iter().any(|c| !c.hotpluggable) {
        let ids: Vec<_> = topo.cpus.iter().filter(|c| !c.hotpluggable).map(|c| format!("cpu{}", c.id)).collect();
        println!("{} cannot be taken offline (no hotplug support)", ids.join(", "));
    }
    if topo.cpus.iter().any(|c| !c.topology_known) {
        println!("note: some offline CPUs have unknown topology (bring all CPUs online once to cache it)");
    }
}

/// Print the result of executing a request. Returns `true` on full success.
pub fn print_outcome(o: &Outcome) -> bool {
    let st = Style::new();
    for w in &o.warnings {
        eprintln!("{} {w}", st.paint("33", "warning:"));
    }
    if o.actions.is_empty() {
        println!("nothing to change");
        return true;
    }
    let verb = if o.dry_run { "would" } else { "did" };
    for a in &o.actions {
        println!("  {verb}: {a}");
    }
    if o.dry_run {
        println!("(dry run: nothing was written)");
        return true;
    }
    for f in &o.failures {
        eprintln!("{} {f}", st.paint("31", "failed:"));
    }
    if let Some(h) = &o.hint {
        eprintln!("hint: {h}");
    }
    println!("{} change(s) applied, {} failed", o.applied, o.failures.len());
    o.failures.is_empty()
}
