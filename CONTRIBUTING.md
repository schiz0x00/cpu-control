# Contributing

This is a personal project maintained in spare time. Issues and pull requests are welcome, but there is no
promised response time.

## The most useful thing you can send

The tool has been used on exactly one machine (see the README's Status section). A report of how it
behaves on yours is worth more than most patches. Open a "Hardware report" issue with the output of:

```
cpu-control status
cpu-control select p; cpu-control select e; cpu-control select lpe; cpu-control select smt
```

Both are read-only and need no root. Say whether the types, cores and SMT threads shown are right for
your CPU.

## Building and testing

```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

Rust 1.95 or newer. The tests need no privileges and no particular hardware: they run against a simulated
sysfs tree in a temp directory (`tests/support/sysfs_tree.rs`) and never touch the real `/sys`.

To try a change by hand without root, use a dry run, which reads the real system and prints what it would
write:

```
cargo run -- -n off e
cargo run -- -n gui
```

## What a change needs

- CI must pass: clippy with warnings denied, and the tests.
- A test for any behaviour change. Unit tests live next to the code; tests of the command line go in
  `tests/cli.rs`. If the change depends on something the kernel does to sysfs, teach the simulated tree to
  do it too, and say in the pull request how you know the kernel does it (source reference or observed
  output).
- Tests must not start the GUI or the TUI, and must not write outside their temp directory.
- If it changes what a user sees or types, update `README.md` and add a line to `CHANGELOG.md`.
- Say what you actually ran. "Tested on a Ryzen 7 7840U as root" and "only `cargo test`" are both fine;
  not saying is not.

There is no `rustfmt` configuration and `cargo fmt` would reformat most of the code, so please don't run
it over files you are not otherwise changing. Match the surrounding style.

## Code that runs as root

Most of this program runs as root for most users. Changes to what gets written, and where, get the closest
review. The rules the code keeps today, which a change should not weaken without discussion:

- never write to a sysfs file that does not already exist;
- validate everything that ends up in a written value or a path;
- a dry run writes nothing;
- never leave the system with zero CPUs online, and bring CPUs online before taking others offline.

## Security issues

Please do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).

## License of contributions

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
project by you, as defined in the Apache-2.0 license, shall be dual licensed as MIT OR Apache-2.0, without
any additional terms or conditions.
