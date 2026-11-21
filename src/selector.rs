//! CPU selector expressions.
//!
//! A comma-separated list of terms evaluated left to right, building a set.
//! A term prefixed with `^` is *subtracted*; every other term is *added*.
//!
//! | term        | meaning                                              |
//! |-------------|------------------------------------------------------|
//! | `N`, `A-B`  | logical CPU ids                                      |
//! | `all`       | every CPU                                            |
//! | `p`         | performance cores (all their threads)                |
//! | `e`         | efficiency cores                                     |
//! | `lpe`       | low-power efficiency cores                           |
//! | `smt`       | second/extra hardware threads of each core           |
//! | `primary`   | first hardware thread of each core                   |
//! | `core:N`    | all threads of the physical core with core_id N      |
//! | `l2:N`      | all CPUs sharing an L2 cache with CPU N              |
//! | `online`    | CPUs currently online                                |
//! | `offline`   | CPUs currently offline                               |
//! | `none`      | empty set                                            |
//!
//! Example: `all,^p` = everything except P-cores; `p,^smt` = one thread per P-core.

use crate::cpuset::CpuSet;
use crate::topology::{CoreKind, Topology};
use anyhow::{Context, Result, bail};

pub fn eval(topo: &Topology, expr: &str) -> Result<CpuSet> {
    let all = topo.all();
    let mut set = CpuSet::new();
    for raw in expr.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (negate, term) = match raw.strip_prefix('^').or_else(|| raw.strip_prefix('!')) {
            Some(rest) => (true, rest.trim()),
            None => (false, raw),
        };
        let term_set = eval_term(topo, term)?;
        if let Some(bad) = term_set.iter().find(|c| !all.contains(*c)) {
            bail!("cpu{bad} does not exist on this system (valid: {all})");
        }
        set = if negate { set.difference(&term_set) } else { set.union(&term_set) };
    }
    Ok(set)
}

fn eval_term(topo: &Topology, term: &str) -> Result<CpuSet> {
    let lower = term.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "all" => topo.all(),
        "none" => CpuSet::new(),
        "p" | "pcore" | "pcores" | "performance" => topo.of_kind(CoreKind::P),
        "e" | "ecore" | "ecores" | "efficiency" => topo.of_kind(CoreKind::E),
        "lpe" | "lp-e" | "lpecore" | "lpecores" => topo.of_kind(CoreKind::LpE),
        "smt" | "ht" | "secondary" => topo.smt_secondaries(),
        "primary" => topo.all().difference(&topo.smt_secondaries()),
        "online" => topo.online(),
        "offline" => topo.all().difference(&topo.online()),
        _ => {
            if let Some(n) = lower.strip_prefix("core:") {
                let id: usize = n.trim().parse().with_context(|| format!("invalid core id in `{term}`"))?;
                // The core_id of an offline CPU with no cached topology is a
                // placeholder and must not be mistaken for a real core.
                let cores: CpuSet = topo
                    .cpus
                    .iter()
                    .filter(|c| c.topology_known && c.core_id == id)
                    .map(|c| c.id)
                    .collect();
                if cores.is_empty() {
                    bail!("no physical core with core_id {id}");
                }
                cores
            } else if let Some(n) = lower.strip_prefix("l2:") {
                let id: usize = n.trim().parse().with_context(|| format!("invalid CPU id in `{term}`"))?;
                match topo.get(id) {
                    Some(c) => c.l2_group.clone(),
                    None => bail!("cpu{id} does not exist"),
                }
            } else {
                // Plain id list / range, e.g. `0-3,8` (single term here).
                match CpuSet::parse_list(term) {
                    Ok(s) => s,
                    Err(_) => bail!(
                        "unknown selector `{term}` (try: N, A-B, all, p, e, lpe, smt, primary, core:N, l2:N, online, offline, none)"
                    ),
                }
            }
        }
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::fake::fake_meteor_lake;

    #[test]
    fn kinds_and_algebra() {
        let (_d, topo) = fake_meteor_lake();
        assert_eq!(eval(&topo, "p").unwrap().to_string(), "0-11");
        assert_eq!(eval(&topo, "e").unwrap().to_string(), "12-19");
        assert_eq!(eval(&topo, "lpe").unwrap().to_string(), "20-21");
        assert_eq!(eval(&topo, "all,^p").unwrap().to_string(), "12-21");
        assert_eq!(eval(&topo, "p,^smt").unwrap().to_string(), "0-1,3,6,8,10");
        assert_eq!(eval(&topo, "core:8").unwrap().to_string(), "1-2");
        assert_eq!(eval(&topo, "l2:13").unwrap().to_string(), "12-15");
        assert_eq!(eval(&topo, "0,^0,5").unwrap().to_string(), "5");
    }

    #[test]
    fn errors() {
        let (_d, topo) = fake_meteor_lake();
        assert!(eval(&topo, "99").is_err());
        assert!(eval(&topo, "bogus").is_err());
        assert!(eval(&topo, "core:9999").is_err());
    }

    fn sel(topo: &Topology, expr: &str) -> String {
        eval(topo, expr).unwrap_or_else(|e| panic!("`{expr}`: {e:#}")).to_string()
    }

    #[test]
    fn every_documented_term() {
        let (_f, topo) = fake_meteor_lake();
        for (expr, want) in [
            ("7", "7"),
            ("3-6", "3-6"),
            ("all", "0-21"),
            ("none", "-"),
            ("p", "0-11"),
            ("e", "12-19"),
            ("lpe", "20-21"),
            ("smt", "2,4-5,7,9,11"),
            ("primary", "0-1,3,6,8,10,12-21"),
            ("core:16", "0,5"),
            ("core:0", "12"),
            ("l2:0", "0,5"),
            ("l2:18", "16-19"),
            ("l2:21", "20-21"),
            ("online", "0-21"),
            ("offline", "-"),
        ] {
            assert_eq!(sel(&topo, expr), want, "`{expr}`");
        }
    }

    #[test]
    fn aliases_case_and_spacing() {
        let (_f, topo) = fake_meteor_lake();
        for (alias, canonical) in [
            ("pcore", "p"),
            ("pcores", "p"),
            ("performance", "p"),
            ("ecore", "e"),
            ("ecores", "e"),
            ("efficiency", "e"),
            ("lp-e", "lpe"),
            ("lpecore", "lpe"),
            ("lpecores", "lpe"),
            ("ht", "smt"),
            ("secondary", "smt"),
            ("P", "p"),
            ("LPE", "lpe"),
            ("All", "all"),
            ("CORE:8", "core:8"),
            ("L2:13", "l2:13"),
            (" p , ^ smt ", "p,^smt"),
            ("p,,", "p"),
            ("all,!p", "all,^p"),
        ] {
            assert_eq!(sel(&topo, alias), sel(&topo, canonical), "`{alias}`");
        }
    }

    #[test]
    fn terms_apply_left_to_right() {
        let (_f, topo) = fake_meteor_lake();
        assert_eq!(sel(&topo, ""), "-");
        assert_eq!(sel(&topo, "^p"), "-"); // nothing to subtract from yet
        assert_eq!(sel(&topo, "^p,e"), "12-19");
        assert_eq!(sel(&topo, "e,^e"), "-");
        assert_eq!(sel(&topo, "e,^e,e"), "12-19");
        assert_eq!(sel(&topo, "all,^p,^lpe"), "12-19");
        assert_eq!(sel(&topo, "all,^3-20"), "0-2,21");
        assert_eq!(sel(&topo, "p,^smt,e"), "0-1,3,6,8,10,12-19");
        assert_eq!(sel(&topo, "all,^none"), "0-21");
        assert_eq!(sel(&topo, "1,1,1"), "1");
    }

    #[test]
    fn online_and_offline_follow_live_state() {
        let (f, _) = fake_meteor_lake();
        f.offline(3);
        f.offline(20);
        let topo = f.discover();
        assert_eq!(sel(&topo, "offline"), "3,20");
        assert_eq!(sel(&topo, "online"), "0-2,4-19,21");
        assert_eq!(sel(&topo, "all,^online"), "3,20");
    }

    #[test]
    fn error_messages_say_what_is_wrong() {
        let (_f, topo) = fake_meteor_lake();
        for (expr, needle) in [
            ("22", "cpu22 does not exist"),
            ("20-25", "does not exist"),
            ("^99", "cpu99 does not exist"), // a typo is an error even when subtracting
            ("bogus", "unknown selector `bogus`"),
            ("p,e,x", "unknown selector `x`"),
            ("3-1", "unknown selector `3-1`"),
            ("core:", "invalid core id"),
            ("core:x", "invalid core id"),
            ("core:-1", "invalid core id"),
            ("core:9999", "no physical core with core_id 9999"),
            ("l2:x", "invalid CPU id"),
            ("l2:40", "cpu40 does not exist"),
            ("0-99999999999", "unknown selector"),
        ] {
            let err = format!("{:#}", eval(&topo, expr).unwrap_err());
            assert!(err.contains(needle), "`{expr}` gave `{err}`, wanted `{needle}`");
        }
    }

    #[test]
    fn core_selector_ignores_placeholder_core_ids() {
        // Offline with no cache, cpu12 reports core_id 12 as a placeholder,
        // which is also the real core_id of the P-core holding cpu3 and cpu4.
        let (f, _) = fake_meteor_lake();
        f.offline(12);
        let topo = f.discover();
        assert_eq!(sel(&topo, "core:12"), "3-4");
        // Its real core (core_id 0) is unknown for now.
        assert!(eval(&topo, "core:0").is_err());
    }
}
