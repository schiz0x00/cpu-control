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
use anyhow::{Result, bail};

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
                let id: usize = n.trim().parse()?;
                let cores: CpuSet = topo
                    .cpus
                    .iter()
                    .filter(|c| c.core_id == id)
                    .map(|c| c.id)
                    .collect();
                if cores.is_empty() {
                    bail!("no physical core with core_id {id}");
                }
                cores
            } else if let Some(n) = lower.strip_prefix("l2:") {
                let id: usize = n.trim().parse()?;
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
}
