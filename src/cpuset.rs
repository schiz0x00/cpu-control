//! A set of logical CPU ids with the kernel's list syntax (`0-3,8,12-15`).

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuSet(BTreeSet<usize>);

impl CpuSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, cpu: usize) {
        self.0.insert(cpu);
    }

    pub fn remove(&mut self, cpu: usize) {
        self.0.remove(&cpu);
    }

    pub fn contains(&self, cpu: usize) -> bool {
        self.0.contains(&cpu)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().copied()
    }

    pub fn union(&self, other: &CpuSet) -> CpuSet {
        CpuSet(self.0.union(&other.0).copied().collect())
    }

    pub fn difference(&self, other: &CpuSet) -> CpuSet {
        CpuSet(self.0.difference(&other.0).copied().collect())
    }

    pub fn intersection(&self, other: &CpuSet) -> CpuSet {
        CpuSet(self.0.intersection(&other.0).copied().collect())
    }

    /// Parse the kernel list format. Empty input yields an empty set.
    /// The kernel's `:stride` suffix (`0-10:2`) is not supported.
    pub fn parse_list(s: &str) -> Result<CpuSet> {
        let mut set = CpuSet::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some((a, b)) = part.split_once('-') {
                let (a, b) = (parse_id(a)?, parse_id(b)?);
                if a > b {
                    bail!("invalid range `{part}`: start is greater than end");
                }
                set.0.extend(a..=b);
            } else {
                set.0.insert(parse_id(part)?);
            }
        }
        Ok(set)
    }
}

/// Largest CPU id accepted when parsing. Far above any real machine (x86-64
/// kernels top out at 8192 CPUs) but small enough that a typo such as
/// `0-99999999999` cannot exhaust memory building the set.
pub const MAX_CPU_ID: usize = 65_535;

fn parse_id(s: &str) -> Result<usize> {
    let s = s.trim();
    let id: usize = s.parse().with_context(|| format!("invalid CPU id `{s}`"))?;
    if id > MAX_CPU_ID {
        bail!("CPU id {id} is out of range (max {MAX_CPU_ID})");
    }
    Ok(id)
}

impl FromIterator<usize> for CpuSet {
    fn from_iter<T: IntoIterator<Item = usize>>(iter: T) -> Self {
        CpuSet(iter.into_iter().collect())
    }
}

impl fmt::Display for CpuSet {
    /// Compact range form, e.g. `0-3,8,12-15`; `-` for the empty set.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return write!(f, "-");
        }
        let mut out = Vec::new();
        let mut iter = self.0.iter().copied().peekable();
        while let Some(start) = iter.next() {
            let mut end = start;
            while iter.peek() == Some(&(end + 1)) {
                end = iter.next().unwrap();
            }
            out.push(match end - start {
                0 => start.to_string(),
                _ => format!("{start}-{end}"),
            });
        }
        write!(f, "{}", out.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_roundtrip() {
        let s = CpuSet::parse_list("0-3, 8,12-15,5").unwrap();
        assert_eq!(s.to_string(), "0-3,5,8,12-15");
        assert_eq!(s.len(), 10);
    }

    #[test]
    fn empty_and_errors() {
        assert!(CpuSet::parse_list("").unwrap().is_empty());
        assert_eq!(CpuSet::new().to_string(), "-");
        assert!(CpuSet::parse_list("3-1").is_err());
        assert!(CpuSet::parse_list("a").is_err());
    }

    #[test]
    fn set_ops() {
        let a = CpuSet::parse_list("0-3").unwrap();
        let b = CpuSet::parse_list("2-5").unwrap();
        assert_eq!(a.intersection(&b).to_string(), "2-3");
        assert_eq!(a.difference(&b).to_string(), "0-1");
        assert_eq!(a.union(&b).to_string(), "0-5");
    }

    fn ids(s: &CpuSet) -> Vec<usize> {
        s.iter().collect()
    }

    #[test]
    fn parse_accepts_the_forms_the_kernel_prints() {
        for (input, want) in [
            ("0", vec![0]),
            ("7\n", vec![7]),
            ("0-0", vec![0]),
            ("0-2", vec![0, 1, 2]),
            ("0,2,4", vec![0, 2, 4]),
            ("3,1,2,1", vec![1, 2, 3]),
            ("0-2,1-4", vec![0, 1, 2, 3, 4]),
            (" 1 - 3 , 9 ", vec![1, 2, 3, 9]),
            (",,5,,", vec![5]),
            ("   ", vec![]),
        ] {
            assert_eq!(ids(&CpuSet::parse_list(input).unwrap()), want, "input {input:?}");
        }
    }

    #[test]
    fn parse_rejects_garbage_with_a_message_naming_it() {
        for bad in ["a", "1-", "-1", "1-2-3", "1.5", "0x10", "1 2", "0-10:2", "p", "-"] {
            let err = CpuSet::parse_list(bad).unwrap_err();
            assert!(format!("{err:#}").contains("invalid"), "{bad:?} gave: {err:#}");
        }
    }

    #[test]
    fn huge_ids_are_rejected_instead_of_materialised() {
        // Without a bound this range would try to allocate billions of entries.
        let started = std::time::Instant::now();
        assert!(CpuSet::parse_list("0-99999999999").unwrap_err().to_string().contains("out of range"));
        assert!(CpuSet::parse_list("18446744073709551615").is_err());
        assert!(CpuSet::parse_list("99999999999999999999999").is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // The bound itself is inclusive.
        assert_eq!(ids(&CpuSet::parse_list(&MAX_CPU_ID.to_string()).unwrap()), [MAX_CPU_ID]);
        assert!(CpuSet::parse_list(&(MAX_CPU_ID + 1).to_string()).is_err());
        assert_eq!(CpuSet::parse_list(&format!("0-{MAX_CPU_ID}")).unwrap().len(), MAX_CPU_ID + 1);
    }

    #[test]
    fn display_compacts_runs() {
        for (members, want) in [
            (vec![], "-"),
            (vec![4], "4"),
            (vec![0, 1], "0-1"),
            (vec![0, 2], "0,2"),
            (vec![0, 1, 2, 4, 6, 7], "0-2,4,6-7"),
            (vec![9, 3, 3, 4], "3-4,9"),
        ] {
            assert_eq!(members.into_iter().collect::<CpuSet>().to_string(), want);
        }
    }

    #[test]
    fn display_then_parse_is_the_identity() {
        // A small deterministic generator: no extra dependencies needed.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..500 {
            let len = next() % 40;
            let set: CpuSet = (0..len).map(|_| (next() % 96) as usize).collect();
            if set.is_empty() {
                continue; // "-" is display-only
            }
            assert_eq!(CpuSet::parse_list(&set.to_string()).unwrap(), set);
        }
    }

    #[test]
    fn insert_remove_contains_len() {
        let mut s = CpuSet::new();
        assert!(s.is_empty() && !s.contains(3));
        assert_eq!(s.len(), 0);
        s.insert(3);
        s.insert(3);
        s.insert(1);
        assert_eq!((s.len(), s.contains(3), s.contains(2)), (2, true, false));
        assert_eq!(ids(&s), [1, 3]); // iteration is ascending
        s.remove(3);
        s.remove(42); // absent: no-op
        assert_eq!(ids(&s), [1]);
    }

    #[test]
    fn set_ops_with_empty_and_disjoint_operands() {
        let a = CpuSet::parse_list("0-3").unwrap();
        let none = CpuSet::new();
        let far = CpuSet::parse_list("8-9").unwrap();
        assert_eq!(a.union(&none), a);
        assert_eq!(a.difference(&none), a);
        assert!(a.intersection(&none).is_empty());
        assert!(none.difference(&a).is_empty());
        assert!(a.intersection(&far).is_empty());
        assert_eq!(a.difference(&far), a);
        assert_eq!(a.union(&far).to_string(), "0-3,8-9");
        assert!(a.difference(&a).is_empty());
    }
}
