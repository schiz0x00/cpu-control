//! Test-only fake sysfs tree; the tree itself lives in
//! `tests/support/sysfs_tree.rs` so the CLI tests can share it.

#[path = "../tests/support/sysfs_tree.rs"]
mod sysfs_tree;

pub use sysfs_tree::{Fake, fake_sys};

use crate::topology::Topology;

impl Fake {
    pub fn discover(&self) -> Topology {
        Topology::discover(&self.root(), &self.devices(), None).unwrap()
    }
}

pub fn fake_meteor_lake() -> (Fake, Topology) {
    let f = fake_sys();
    let t = f.discover();
    (f, t)
}
