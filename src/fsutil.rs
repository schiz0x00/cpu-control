//! Small filesystem helpers shared by the profile store and the topology cache.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, fchown};
use std::path::Path;

/// Replace `path` with `contents` without ever exposing a half-written file:
/// write a sibling temp file, then rename it into place. A reader (or a crash)
/// sees either the old contents or the new ones.
///
/// A symlinked `path` keeps pointing where it did (the file it points at is
/// what gets replaced), and an existing file keeps its owner and mode.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    // Follow an existing symlink so dotfile-style setups survive the rename.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file path"))?;
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir)?;

    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    // Only a crashed earlier run with our pid can have left this behind.
    let _ = fs::remove_file(&tmp);

    let write = || -> io::Result<()> {
        // `create_new` is O_EXCL: it never follows a symlink planted at `tmp`.
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        if let Ok(old) = fs::metadata(&target) {
            // Best effort: only root can give a file away.
            let _ = fchown(&f, Some(old.uid()), Some(old.gid()));
            f.set_permissions(fs::Permissions::from_mode(old.mode() & 0o7777))?;
        }
        f.sync_all()?;
        fs::rename(&tmp, &target)
    };
    let res = write();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }

    #[test]
    fn creates_parent_dirs_and_leaves_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b/file.toml");
        write_atomic(&p, "one").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        write_atomic(&p, "two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(entries(p.parent().unwrap()), ["file.toml"]);
    }

    #[test]
    fn keeps_the_mode_of_the_file_it_replaces() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&p, "new").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(fs::read_to_string(&p).unwrap(), "new");
    }

    #[test]
    fn writes_through_a_symlink_instead_of_replacing_it() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("dotfiles/profiles.toml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "old").unwrap();
        let link = d.path().join("profiles.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, "new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    }

    #[test]
    fn a_dangling_symlink_is_replaced_by_a_regular_file() {
        let d = tempfile::tempdir().unwrap();
        let link = d.path().join("f");
        std::os::unix::fs::symlink(d.path().join("missing/target"), &link).unwrap();
        write_atomic(&link, "x").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_file());
        assert!(!d.path().join("missing").exists());
    }

    #[test]
    fn rejects_a_path_without_a_file_name() {
        assert!(write_atomic(Path::new("/"), "x").is_err());
    }

    #[test]
    fn a_failed_write_leaves_the_old_file_and_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        // The destination is a non-empty directory, so the final rename fails.
        let p = d.path().join("taken");
        fs::create_dir_all(p.join("child")).unwrap();
        assert!(write_atomic(&p, "x").is_err());
        assert_eq!(entries(d.path()), ["taken"]);
    }
}
