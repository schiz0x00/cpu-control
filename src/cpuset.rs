//! A set of logical CPU ids with the kernel's list syntax (`0-3,8,12-15`).

use anyhow::{Result, bail};
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
    pub fn parse_list(s: &str) -> Result<CpuSet> {
        let mut set = CpuSet::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            // Allow an optional `:stride` suffix is NOT supported; keep it strict.
            if let Some((a, b)) = part.split_once('-') {
                let (a, b): (usize, usize) = (a.trim().parse()?, b.trim().parse()?);
                if a > b {
                    bail!("invalid range `{part}`: start is greater than end");
                }
                set.0.extend(a..=b);
            } else {
                set.0.insert(part.parse()?);
            }
        }
        Ok(set)
    }
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
}
