# cpu-control

Granular CPU core control for Linux (built around hybrid Intel CPUs, works on any sysfs-hotplug system).
Offline arbitrary threads, whole core classes (P / E / LP-E), SMT siblings, L2 clusters; set governors,
energy preference and frequency caps; save and apply profiles.

```
cargo build --release
./target/release/cpu-control          # opens the GUI (same as `cpu-control gui`)
sudo ./target/release/cpu-control preset no-pcores
```

## GUI

Run it as your normal user. Cores are laid out as cards grouped by type (P / E / LP-E), one button per
hardware thread with a live frequency bar. Click a thread to toggle it, `⏻` toggles a whole core, and each
section has *Enable/Disable all* and *Toggle SMT threads*. The side panel sets governor, energy preference and
min/max frequency for the selected CPU, its core, its type, or everything. Presets are in the **Presets** menu.

Everything is *staged* (yellow outline) until you press **Apply**. When you aren't root, Apply runs
`pkexec cpu-control preset <staged profile>`, so you get a normal polkit password prompt and only the
validated CLI path runs privileged. `-n` makes the GUI a dry run.

Reading (`status`, `select`) and `--dry-run` / `-n` need no privileges; changes write to
`/sys/devices/system/cpu` and need root.

## Commands

| command | what it does |
|---|---|
| `status` | table of every CPU: type, core, thread, L2 cluster, state, freq, governor |
| `off <sel>` / `on <sel>` | hotplug the selected CPUs |
| `only <sel>` | exactly these CPUs online, everything else offline |
| `preset [name]` | apply a built-in/saved preset, or list them |
| `set <sel> --governor G --epp E --min F --max F` | per-CPU tuning (`--reset-freq` restores HW limits) |
| `turbo on\|off`, `smt on\|off` | global switches |
| `profile save\|apply\|show\|delete\|list` | snapshot live state / manage saved profiles |
| `select <sel>` | print what a selector resolves to |
| `tui` | interactive UI; changes are staged, `w` applies |

## Selectors

Comma-separated terms, evaluated left to right, `^` subtracts:
`N`, `A-B`, `all`, `p`, `e`, `lpe`, `smt` (extra hardware threads), `primary`, `core:N`, `l2:N`, `online`, `offline`, `none`.

```
cpu-control off 'p'              # disable all P-cores
cpu-control only 'p,^smt'        # one thread per P-core, nothing else
cpu-control set e --max 2ghz     # cap E-cores at 2 GHz
cpu-control off 'l2:12'          # drop an entire E-core cluster
```

## Built-in presets

`all`, `no-pcores`, `no-ecores`, `no-lpe`, `pcores-only`, `ecores-only`, `lpe-only`, `no-smt`,
`pcores-no-smt`, `max-perf`, `quiet`.

## Custom profiles (`~/.config/cpu-control/profiles.toml`)

```toml
[profile.gaming]
description = "P-cores w/o SMT, E-cores capped"
online = "p,^smt,e"       # exactly these end up online
turbo = true
governor = "performance"  # default for every online CPU
epp = "performance"

[[profile.gaming.tune]]   # overrides
cpus = "e"
max_freq = "2.4ghz"
```

## Notes

- **CPU 0 is usually not hot-pluggable** on x86 (no `online` file). It's reported as `fixed`, left online, and
  presets like `no-pcores` warn about it rather than failing.
- The kernel deletes `topology/` and `cache/` for offline CPUs, so the tool caches core/SMT/L2/type info
  (`/var/lib/cpu-control/topology.toml`, or `~/.local/state/cpu-control/`) while CPUs are online. If you start
  with CPUs already offline and no cache, they show `?` until you bring everything online once.
- Hotplug state is not persistent across reboots; apply a preset from a boot-time unit if you want it to stick.
- The tool never lets the online set become empty, and brings CPUs online before taking others offline.
- Hotplug is a heavy-handed tool: offlining a core migrates its tasks and resets its cpufreq settings
  when brought back, which is why tuning is applied after hotplug.
