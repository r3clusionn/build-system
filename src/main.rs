use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};

// Output that does not panic when the reader goes away (`kiln explain | head`); the build itself
// carries on.
macro_rules! out {
    ($($t:tt)*) => {{
        use std::io::Write;
        let _ = write!(std::io::stdout(), $($t)*);
    }};
}

macro_rules! outln {
    ($($t:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($t)*);
    }};
}
use kiln::config;
use kiln::exec::{self, Event, Options};
use kiln::graph::Graph;
use kiln::state::State;

#[derive(Parser)]
#[command(name = "kiln", version, about = "A build system with content-hashed incremental builds")]
struct Cli {
    /// The build file
    #[arg(short, long, default_value = "kiln.toml", global = true)]
    file: PathBuf,
    /// The profile to build (default: `debug` if the file has one, else the first)
    #[arg(short, long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Targets for the default `build` command
    targets: Vec<String>,
    /// Commands to run at once (default: one per logical CPU)
    #[arg(short, long)]
    jobs: Option<usize>,
    /// Keep building what does not depend on a failed step
    #[arg(short, long)]
    keep_going: bool,
    /// Print every command
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build targets (task names or output files; default: the default tasks, else everything)
    Build {
        targets: Vec<String>,
        #[arg(short, long)]
        jobs: Option<usize>,
        #[arg(short, long)]
        keep_going: bool,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Say which steps would run and why, without running anything
    Explain { targets: Vec<String> },
    /// Delete the outputs of the targets
    Clean { targets: Vec<String> },
    /// List the tasks and how many steps each has
    Targets,
    /// Print the step graph as Graphviz DOT
    Graph { targets: Vec<String> },
}

struct Project {
    root: PathBuf,
    graph: Graph,
    defaults: Vec<String>,
    profile: String,
    state_path: PathBuf,
}

fn load(file: &Path, profile: Option<&str>) -> Result<Project, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let bf = config::parse(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let root = file.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let profile = match profile {
        Some(p) => p.to_string(),
        None if bf.profile.contains_key("debug") => "debug".into(),
        None => bf.profile.keys().next().cloned().unwrap_or_else(|| "default".into()),
    };
    let steps = config::expand(&bf, &root, &profile)?;
    let defaults = bf.tasks.iter().filter(|t| t.default).map(|t| t.name.clone()).collect();
    let graph = Graph::new(steps)?;
    Ok(Project { state_path: root.join(".kiln").join("state.json"), root, graph, defaults, profile })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("kiln: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let cmd = cli.command.unwrap_or(Cmd::Build {
        targets: cli.targets,
        jobs: cli.jobs,
        keep_going: cli.keep_going,
        verbose: cli.verbose,
    });
    let p = load(&cli.file, cli.profile.as_deref())?;
    match cmd {
        Cmd::Build { targets, jobs, keep_going, verbose } => {
            let selected = p.graph.select(&targets, &p.defaults)?;
            let mut state = State::load(&p.state_path);
            let opts = Options { jobs: jobs.unwrap_or(Options::default().jobs), keep_going, dry_run: false, verbose };
            let t = Instant::now();
            let r = exec::build(&p.root, &p.graph, &selected, &mut state, &opts, &mut |e| match e {
                Event::Ran { done, total, description, command, output, .. } => {
                    outln!("[{done}/{total}] {description}");
                    if verbose {
                        outln!("  {command}");
                    }
                    if !output.trim().is_empty() {
                        out!("{output}");
                    }
                }
                Event::Failed { description, command, output, code } => {
                    outln!("FAILED: {description} (exit {})", code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()));
                    outln!("  {command}");
                    out!("{output}");
                }
            });
            // Save what completed, also when the build stopped with an error.
            if r.as_ref().map_or(true, |r| r.state_changed) {
                state.save(&p.state_path).map_err(|e| format!("{}: {e}", p.state_path.display()))?;
            }
            let r = r?;
            let secs = t.elapsed().as_secs_f64();
            if r.ran == 0 && r.failed == 0 {
                outln!(
                    "kiln: {} up to date ({} steps checked in {:.3} s, profile {})",
                    if selected.len() == 1 { "1 step" } else { "everything" },
                    r.up_to_date,
                    secs,
                    p.profile
                );
            } else {
                outln!(
                    "kiln: {} run, {} up to date{}{} in {:.2} s (profile {})",
                    r.ran,
                    r.up_to_date,
                    if r.failed > 0 { format!(", {} failed", r.failed) } else { String::new() },
                    if r.skipped > 0 { format!(", {} not built", r.skipped) } else { String::new() },
                    secs,
                    p.profile
                );
            }
            Ok(r.ok())
        }
        Cmd::Explain { targets } => {
            let selected = p.graph.select(&targets, &p.defaults)?;
            let mut state = State::load(&p.state_path);
            let opts = Options { dry_run: true, ..Options::default() };
            let r = exec::build(&p.root, &p.graph, &selected, &mut state, &opts, &mut |e| {
                if let Event::Ran { description, reason, .. } = e {
                    outln!("{description}: {reason}");
                }
            })?;
            outln!("kiln: {} would run, {} up to date", r.ran, r.up_to_date);
            Ok(true)
        }
        Cmd::Clean { targets } => {
            let selected = p.graph.select(&targets, &[])?;
            let mut state = State::load(&p.state_path);
            let n = exec::clean(&p.root, &p.graph, &selected, &mut state);
            state.save(&p.state_path).map_err(|e| e.to_string())?;
            outln!("kiln: removed {n} files");
            Ok(true)
        }
        Cmd::Targets => {
            let mut tasks: Vec<(String, usize)> = Vec::new();
            for s in &p.graph.steps {
                match tasks.iter_mut().find(|t| t.0 == s.task) {
                    Some(t) => t.1 += 1,
                    None => tasks.push((s.task.clone(), 1)),
                }
            }
            for (t, n) in tasks {
                let star = if p.defaults.contains(&t) { " (default)" } else { "" };
                outln!("{t}{star}: {n} step{}", if n == 1 { "" } else { "s" });
            }
            Ok(true)
        }
        Cmd::Graph { targets } => {
            let selected = p.graph.select(&targets, &p.defaults)?;
            out!("{}", p.graph.dot(&selected));
            Ok(true)
        }
    }
}
