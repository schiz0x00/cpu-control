# Changelog

Notable changes to this project. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

No version has been tagged or released yet. `0.1.0` in `Cargo.toml` is the working version, and everything
below is what `main` contains beyond the first public commit.

## Unreleased

### Fixed

- SMT and L2 groups stay whole when a sibling is offline. The kernel drops an offline CPU from its
  relatives' lists, and those shrunken lists used to be believed and cached; `only p,^smt` could then keep
  both threads of a core online.
- Applying a plan no longer creates files: it writes only to sysfs attributes that already exist.
- Once a CPU has failed to come online, no CPU is taken offline in that run.
- Under `sudo`, a profiles path containing `..` could make the tool change ownership of directories
  outside the invoking user's home. Such paths are no longer walked, and ownership changes no longer
  follow symlinks.
- A selector such as `0-99999999999` no longer hangs and exhausts memory.
- Frequencies such as `inf` or `0.0001` are rejected instead of being written as nonsense values.
- Profile defaults (`governor`, `epp`, `min_freq`, `max_freq`) now also reach a CPU that cannot be taken
  offline, matching what the GUI and TUI already did.
- A profile's tune entries are checked against the entries before them, not against the live limits those
  entries replace.
- Profiles and the topology cache are written atomically, and the cache is rewritten only when it changes.
- A dry run no longer writes the topology cache.
- An offline CPU whose topology is unknown is shown as `?` and grouped on its own, instead of being shown
  as a P-core and matched by `core:N` through a placeholder id.
- `status`, the GUI and the TUI show turbo state on systems that use `cpufreq/boost`.
- TUI: stepping the max frequency for a CPU staged to come online started from zero; staged edits were
  discarded after a failed apply; starting without a terminal panicked.
- GUI and TUI: a `profiles.toml` that fails to parse is reported instead of silently hiding saved profiles.
- GUI: core cards that did not fit the width of the window ran off its edge instead of wrapping onto a new
  row, where they could not be seen or clicked. On a 6 P-core machine at the default window size that hid
  the sixth P-core.
- A sysfs tree that lists no CPUs is an error instead of an empty table.

### Changed

- Governor and energy-preference values must be plain names (letters, digits, `_`, `-`).
- CPU ids above 65535 and frequencies outside 1 kHz to 100 GHz are rejected.
- Unknown top-level keys in `profiles.toml` are an error.
- Hotplug commands warn when the topology of an offline CPU is not known.

### Added

- Test suite: unit tests and end-to-end tests of the command line against a simulated sysfs tree.
- CI (clippy and tests), contribution and security documents, issue templates.
- Licensed as MIT OR Apache-2.0.
