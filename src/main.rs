mod ctx;
mod cpuset;
mod gui;
#[cfg(test)]
mod fake;
mod output;
mod plan;
mod profile;
mod selector;
mod topology;
mod tui;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use ctx::Ctx;
use plan::{Request, Tune, parse_freq_khz};
use profile::ProfileFile;
use std::path::PathBuf;
use std::process::ExitCode;

/// Granular control over CPU cores: hotplug individual threads or whole core
/// classes (P / E / LP-E), tune frequency limits and governors, save profiles.
///
/// Changing state writes to /sys and needs root (use sudo). `status`, `select`
/// and `--dry-run` work unprivileged.
///
/// Selectors (used by most commands) are comma-separated terms evaluated left
/// to right; prefix a term with ^ to subtract it:
///   N  A-B  all  p  e  lpe  smt  primary  core:N  l2:N  online  offline  none
/// e.g. `all,^p` (everything but P-cores), `p,^smt` (one thread per P-core).
#[derive(Parser)]
#[command(version, about, verbatim_doc_comment)]
struct Cli {
    /// Show what would be done without writing anything.
    #[arg(short = 'n', long, global = true)]
    dry_run: bool,

    /// Use a different sysfs CPU directory (for testing).
    #[arg(long, global = true, hide = true, value_name = "DIR")]
    sysfs_root: Option<PathBuf>,

    /// Profiles file (default: ~/.config/cpu-control/profiles.toml).
    #[arg(long, global = true, value_name = "FILE")]
    profiles: Option<PathBuf>,

    /// Defaults to the graphical interface.
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
}

impl Switch {
    fn is_on(self) -> bool {
        matches!(self, Switch::On)
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Show every CPU: type, topology, state, frequency, governor.
    #[command(alias = "list", alias = "ls")]
    Status,
    /// Print the CPUs a selector expression resolves to.
    Select { selector: String },
    /// Bring the selected CPUs online.
    On { selector: String },
    /// Take the selected CPUs offline.
    Off { selector: String },
    /// Make exactly the selected CPUs online; offline everything else.
    Only { selector: String },
    /// Apply a built-in or saved preset (no name: list them).
    Preset { name: Option<String> },
    /// Set governor / energy preference / frequency limits on selected CPUs.
    Set {
        selector: String,
        #[arg(long, short)]
        governor: Option<String>,
        /// Energy performance preference (e.g. performance, balance_power, power).
        #[arg(long, short)]
        epp: Option<String>,
        /// Minimum frequency (bare number = MHz; or 800mhz, 1.2ghz).
        #[arg(long)]
        min: Option<String>,
        /// Maximum frequency.
        #[arg(long)]
        max: Option<String>,
        /// Restore the hardware min/max limits.
        #[arg(long, conflicts_with_all = ["min", "max"])]
        reset_freq: bool,
    },
    /// Enable or disable turbo / boost globally.
    Turbo { state: Switch },
    /// Enable or disable SMT (hyper-threading) globally via the kernel switch.
    Smt { state: Switch },
    /// Manage saved profiles.
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Interactive terminal UI.
    Tui,
    /// Graphical interface (default when no command is given).
    Gui,
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List built-in and saved profiles.
    List,
    /// Show the definition of a profile as TOML.
    Show { name: String },
    /// Save the current live state as a profile.
    Save {
        name: String,
        #[arg(long, short)]
        description: Option<String>,
    },
    /// Apply a profile (same as `preset`).
    Apply { name: String },
    /// Delete a saved profile.
    Delete { name: String },
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    let ctx = Ctx::new(cli.sysfs_root, cli.profiles, cli.dry_run);
    let (topo, globals) = ctx.discover()?;

    let exec = |req: Request| -> Result<bool> {
        let outcome = ctx.execute(&topo, &req)?;
        Ok(output::print_outcome(&outcome))
    };

    match cli.command.unwrap_or(Cmd::Gui) {
        Cmd::Status => {
            output::print_status(&topo, &globals);
            Ok(true)
        }
        Cmd::Select { selector } => {
            let set = selector::eval(&topo, &selector)?;
            println!("{set}");
            Ok(true)
        }
        Cmd::On { selector } => exec(Request { online: selector::eval(&topo, &selector)?, ..Default::default() }),
        Cmd::Off { selector } => exec(Request { offline: selector::eval(&topo, &selector)?, ..Default::default() }),
        Cmd::Only { selector } => {
            let set = selector::eval(&topo, &selector)?;
            if set.is_empty() {
                bail!("selector `{selector}` matched no CPUs");
            }
            exec(Request { offline: topo.all().difference(&set), online: set, ..Default::default() })
        }
        Cmd::Preset { name: None } | Cmd::Profile(ProfileCmd::List) => {
            list_profiles(&ctx)?;
            Ok(true)
        }
        Cmd::Preset { name: Some(name) } | Cmd::Profile(ProfileCmd::Apply { name }) => {
            let file = ProfileFile::load(&ctx.profiles_path)?;
            let Some(p) = profile::lookup(&file, &name) else {
                bail!("no preset or profile named `{name}` (see `cpu-control preset`)");
            };
            exec(profile::to_request(&topo, &p).with_context(|| format!("preset `{name}`"))?)
        }
        Cmd::Set { selector, governor, epp, min, max, reset_freq } => {
            let cpus = selector::eval(&topo, &selector)?;
            if cpus.is_empty() {
                bail!("selector `{selector}` matched no CPUs");
            }
            let mut tune = Tune { cpus, governor, epp, ..Default::default() };
            tune.min_khz = min.as_deref().map(parse_freq_khz).transpose()?;
            tune.max_khz = max.as_deref().map(parse_freq_khz).transpose()?;
            if reset_freq {
                // Per-CPU hardware limits differ on hybrid parts, so expand per CPU.
                let mut reqs = Vec::new();
                for id in tune.cpus.iter() {
                    let c = topo.get(id).unwrap();
                    reqs.push(Tune {
                        cpus: [id].into_iter().collect(),
                        min_khz: c.min_khz,
                        max_khz: c.max_khz,
                        governor: tune.governor.clone(),
                        epp: tune.epp.clone(),
                    });
                }
                return exec(Request { tune: reqs, ..Default::default() });
            }
            if tune.is_noop() {
                bail!("nothing to set: pass --governor, --epp, --min, --max or --reset-freq");
            }
            exec(Request { tune: vec![tune], ..Default::default() })
        }
        Cmd::Turbo { state } => exec(Request { turbo: Some(state.is_on()), ..Default::default() }),
        Cmd::Smt { state } => exec(Request { smt: Some(state.is_on()), ..Default::default() }),
        Cmd::Profile(ProfileCmd::Show { name }) => {
            let file = ProfileFile::load(&ctx.profiles_path)?;
            let Some(p) = profile::lookup(&file, &name) else { bail!("no profile named `{name}`") };
            let mut one = ProfileFile::default();
            one.profile.insert(name, p);
            print!("{}", toml::to_string_pretty(&one)?);
            Ok(true)
        }
        Cmd::Profile(ProfileCmd::Save { name, description }) => {
            let mut file = ProfileFile::load(&ctx.profiles_path)?;
            let mut p = profile::snapshot(&topo, &globals);
            if description.is_some() {
                p.description = description;
            }
            if ctx.dry_run {
                println!("(dry run) would save profile `{name}` to {}", ctx.profiles_path.display());
                return Ok(true);
            }
            file.profile.insert(name.clone(), p.clone());
            file.save(&ctx.profiles_path)?;
            ctx::fix_ownership(&ctx.profiles_path);
            println!("saved profile `{name}` to {}\n  {}", ctx.profiles_path.display(), profile::describe(&p));
            Ok(true)
        }
        Cmd::Profile(ProfileCmd::Delete { name }) => {
            let mut file = ProfileFile::load(&ctx.profiles_path)?;
            if file.profile.remove(&name).is_none() {
                bail!("no saved profile named `{name}`");
            }
            if ctx.dry_run {
                println!("(dry run) would delete profile `{name}`");
                return Ok(true);
            }
            file.save(&ctx.profiles_path)?;
            ctx::fix_ownership(&ctx.profiles_path);
            println!("deleted profile `{name}`");
            Ok(true)
        }
        Cmd::Gui => {
            gui::run(ctx.clone(), topo, globals)?;
            Ok(true)
        }
        Cmd::Tui => {
            tui::run(&ctx, topo, globals)?;
            Ok(true)
        }
    }
}

fn list_profiles(ctx: &Ctx) -> Result<()> {
    let file = ProfileFile::load(&ctx.profiles_path)?;
    println!("Built-in presets:");
    for (name, p) in profile::builtin() {
        if file.profile.contains_key(name) {
            continue; // shadowed below
        }
        println!("  {name:<14} {}", p.description.as_deref().unwrap_or(""));
    }
    if !file.profile.is_empty() {
        println!("\nSaved profiles ({}):", ctx.profiles_path.display());
        for (name, p) in &file.profile {
            let d = p.description.clone().unwrap_or_else(|| profile::describe(p));
            println!("  {name:<14} {d}");
        }
    }
    Ok(())
}
