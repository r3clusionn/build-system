//! Running a build: each selected step starts when the steps it depends on are done, decides
//! from content hashes whether it must run, runs its command if so, and records what it read and
//! wrote. Up to `jobs` commands run at once.
//!
//! A step's dependents decide only after its outputs are hashed, so a step that runs again but
//! writes the same bytes does not make anything after it run (early cutoff).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;

use crate::config::Step;
use crate::depfile;
use crate::graph::{norm, Graph};
use crate::state::{key_path, step_key, Hasher, State, StepRecord};

#[derive(Clone, Debug)]
pub struct Options {
    pub jobs: usize,
    pub keep_going: bool,
    /// Decide and report, but run nothing.
    pub dry_run: bool,
    /// Print every command.
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            jobs: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            keep_going: false,
            dry_run: false,
            verbose: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A step ran (or would run, in a dry run): position among the selected steps, reason, time.
    Ran {
        done: usize,
        total: usize,
        description: String,
        command: String,
        reason: String,
        output: String,
        ms: u128,
    },
    Failed {
        description: String,
        command: String,
        output: String,
        code: Option<i32>,
    },
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcome {
    pub ran: usize,
    pub up_to_date: usize,
    pub failed: usize,
    /// Not attempted because something they need failed.
    pub skipped: usize,
    /// The state changed and should be saved.
    pub state_changed: bool,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.failed == 0 && self.skipped == 0
    }
}

enum Decision {
    UpToDate,
    Run { reason: String },
}

enum Done {
    UpToDate,
    Ran { reason: String, record: Option<StepRecord>, output: String, ms: u128 },
    Failed { output: String, code: Option<i32> },
}

/// The inputs of a step with their current hashes (declared, plus discovered last time).
/// A declared input that is missing and that no step produces is an error.
fn current_inputs(step: &Step, rec: Option<&StepRecord>, hasher: &Hasher) -> Result<BTreeMap<String, String>, String> {
    let mut m = BTreeMap::new();
    for i in &step.inputs {
        match hasher.hash(i) {
            Some(h) => m.insert(key_path(i), h),
            None => return Err(format!("`{}` needs `{}`, which does not exist", step.description, i.display())),
        };
    }
    if let Some(r) = rec {
        for d in &r.discovered {
            m.entry(d.clone()).or_insert_with(|| hasher.hash(Path::new(d)).unwrap_or_else(|| "missing".into()));
        }
    }
    Ok(m)
}

fn decide(step: &Step, rec: Option<&StepRecord>, inputs: &BTreeMap<String, String>, hasher: &Hasher) -> Decision {
    let Some(r) = rec else { return Decision::Run { reason: "never built".into() } };
    if r.command != step.command {
        return Decision::Run { reason: "command changed".into() };
    }
    for (p, h) in inputs {
        match r.inputs.get(p) {
            None => return Decision::Run { reason: format!("new input {p}") },
            Some(old) if old != h => return Decision::Run { reason: format!("{p} changed") },
            _ => {}
        }
    }
    if let Some(p) = r.inputs.keys().find(|p| !inputs.contains_key(*p)) {
        return Decision::Run { reason: format!("input {p} removed") };
    }
    for o in &step.outputs {
        let k = key_path(o);
        match (hasher.hash(o), r.outputs.get(&k)) {
            (None, _) => return Decision::Run { reason: format!("{k} is missing") },
            (Some(h), Some(old)) if h != *old => return Decision::Run { reason: format!("{k} was modified") },
            (Some(_), None) => return Decision::Run { reason: format!("{k} is new") },
            _ => {}
        }
    }
    if r.key != step_key(&step.command, &step.outputs, inputs) {
        return Decision::Run { reason: "outputs renamed".into() };
    }
    Decision::UpToDate
}

/// Splits off the program of a command line: a quoted or bare first word.
pub fn split_program(command: &str) -> (String, &str) {
    let c = command.trim_start();
    if let Some(rest) = c.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (rest[..end].to_string(), &rest[end + 1..]);
        }
    }
    match c.find([' ', '\t']) {
        Some(i) => (c[..i].to_string(), &c[i..]),
        None => (c.to_string(), ""),
    }
}

/// True when a command needs the shell: redirection, pipes, chaining, variables, or a program
/// name that may be a shell builtin (no extension and no path).
#[cfg(windows)]
fn needs_shell(command: &str) -> bool {
    if command.contains(['|', '&', '<', '>', '^', '%']) {
        return true;
    }
    let (prog, _) = split_program(command);
    let builtins = [
        "echo", "copy", "del", "erase", "mkdir", "md", "rmdir", "rd", "move", "ren", "rename", "type", "cd", "set", "if", "for",
        "call", "dir", "cls", "exit", "mklink",
    ];
    builtins.contains(&prog.to_ascii_lowercase().as_str())
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    use std::os::windows::process::CommandExt;
    if !needs_shell(command) {
        // Like Ninja: start the program directly with the command line as written, which saves
        // starting cmd.exe for every step.
        let (prog, rest) = split_program(command);
        let mut c = Command::new(prog);
        c.raw_arg(rest.trim_start());
        return c;
    }
    let mut c = Command::new("cmd");
    // Passed verbatim: cmd's own quoting rules, not the C runtime's.
    c.raw_arg(format!("/S /C \"{command}\""));
    c
}

#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    let _ = split_program;
    let mut c = Command::new("sh");
    c.arg("-c").arg(command);
    c
}

fn run_step(root: &Path, step: &Step, rec: Option<StepRecord>, hasher: &Hasher, dry_run: bool) -> Result<Done, String> {
    let inputs = current_inputs(step, rec.as_ref(), hasher)?;
    let reason = match decide(step, rec.as_ref(), &inputs, hasher) {
        Decision::UpToDate => return Ok(Done::UpToDate),
        Decision::Run { reason } => reason,
    };
    if dry_run {
        return Ok(Done::Ran { reason, record: None, output: String::new(), ms: 0 });
    }
    for p in step.outputs.iter().chain(step.depfile.iter()) {
        if let Some(d) = root.join(p).parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
    }
    if let Some(d) = &step.depfile {
        let _ = std::fs::remove_file(root.join(d));
    }
    let t = Instant::now();
    let out = shell(&step.command)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("`{}`: cannot start the shell: {e}", step.description))?;
    let ms = t.elapsed().as_millis();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Ok(Done::Failed { output: text, code: out.status.code() });
    }
    let mut outputs = BTreeMap::new();
    for o in &step.outputs {
        hasher.forget(o);
        match hasher.hash(o) {
            Some(h) => outputs.insert(key_path(o), h),
            None => {
                return Ok(Done::Failed {
                    output: format!("{text}kiln: the command did not create {}\n", o.display()),
                    code: None,
                })
            }
        };
    }
    let mut discovered = Vec::new();
    if let Some(d) = &step.depfile {
        let body =
            std::fs::read_to_string(root.join(d)).map_err(|e| format!("`{}`: depfile {}: {e}", step.description, d.display()))?;
        for p in depfile::parse(&body) {
            let p = if p.is_absolute() { p } else { norm(&p) };
            let k = key_path(&p);
            if !step.inputs.iter().any(|i| key_path(i) == k) && !discovered.contains(&k) {
                discovered.push(k);
            }
        }
    }
    // The record holds what the step read this time, discovered inputs included.
    let mut all = BTreeMap::new();
    for i in &step.inputs {
        all.insert(key_path(i), hasher.hash(i).unwrap_or_else(|| "missing".into()));
    }
    for d in &discovered {
        all.insert(d.clone(), hasher.hash(Path::new(d)).unwrap_or_else(|| "missing".into()));
    }
    let record = StepRecord {
        key: step_key(&step.command, &step.outputs, &all),
        command: step.command.clone(),
        inputs: all,
        outputs,
        discovered,
    };
    Ok(Done::Ran { reason, record: Some(record), output: text, ms })
}

/// Builds the selected steps. `report` sees every step that ran or failed, in completion order.
pub fn build(
    root: &Path,
    graph: &Graph,
    selected: &[usize],
    state: &mut State,
    opts: &Options,
    report: &mut dyn FnMut(Event),
) -> Result<Outcome, String> {
    let hasher = Hasher::new(root, std::mem::take(&mut state.files));
    let total = selected.len();
    let mut in_set = vec![false; graph.steps.len()];
    for i in selected {
        in_set[*i] = true;
    }
    let mut waiting: Vec<usize> = (0..graph.steps.len()).map(|i| graph.deps[i].iter().filter(|d| in_set[**d]).count()).collect();
    let mut users = vec![Vec::new(); graph.steps.len()];
    for i in selected {
        for d in &graph.deps[*i] {
            if in_set[*d] {
                users[*d].push(*i);
            }
        }
    }
    let mut ready: Vec<usize> = selected.iter().copied().filter(|i| waiting[*i] == 0).collect();
    ready.reverse();
    let mut out = Outcome::default();
    let mut blocked = vec![false; graph.steps.len()];
    // In a dry run, everything after a step that would run would run too.
    let mut would_run = vec![false; graph.steps.len()];
    let mut error: Option<String> = None;
    let pool = workpool::ThreadPool::new(opts.jobs.max(1));
    let (tx, rx) = mpsc::channel::<(usize, Result<Done, String>)>();
    let hasher = &hasher;
    let mut running = 0usize;
    let mut finished = 0usize;
    let stop = |out: &Outcome, error: &Option<String>| error.is_some() || (!opts.keep_going && out.failed > 0);
    pool.handle().scope(|scope| {
        loop {
            while running < opts.jobs.max(1) && !stop(&out, &error) {
                let Some(i) = ready.pop() else { break };
                let step = &graph.steps[i];
                if opts.dry_run && graph.deps[i].iter().any(|d| would_run[*d]) {
                    would_run[i] = true;
                    out.ran += 1;
                    finished += 1;
                    report(Event::Ran {
                        done: finished,
                        total,
                        description: step.description.clone(),
                        command: step.command.clone(),
                        reason: "a step it depends on would run".into(),
                        output: String::new(),
                        ms: 0,
                    });
                    for u in &users[i] {
                        waiting[*u] -= 1;
                        if waiting[*u] == 0 {
                            ready.push(*u);
                        }
                    }
                    continue;
                }
                let rec = step.outputs.first().and_then(|o| state.steps.get(&key_path(o)).cloned());
                let tx = tx.clone();
                let dry = opts.dry_run;
                running += 1;
                scope.spawn(move || {
                    let r = run_step(root, step, rec, hasher, dry);
                    let _ = tx.send((i, r));
                });
            }
            if running == 0 {
                break;
            }
            let (i, r) = rx.recv().expect("worker lost");
            running -= 1;
            finished += 1;
            let step = &graph.steps[i];
            let ok = match r {
                Err(e) => {
                    error.get_or_insert(e);
                    false
                }
                Ok(Done::UpToDate) => {
                    out.up_to_date += 1;
                    true
                }
                Ok(Done::Ran { reason, record, output, ms }) => {
                    out.ran += 1;
                    would_run[i] = true;
                    if let (Some(rec), Some(o)) = (record, step.outputs.first()) {
                        state.steps.insert(key_path(o), rec);
                    }
                    report(Event::Ran {
                        done: finished,
                        total,
                        description: step.description.clone(),
                        command: step.command.clone(),
                        reason,
                        output,
                        ms,
                    });
                    true
                }
                Ok(Done::Failed { output, code }) => {
                    out.failed += 1;
                    // A failed step must run again next time.
                    if let Some(o) = step.outputs.first() {
                        state.steps.remove(&key_path(o));
                    }
                    report(Event::Failed { description: step.description.clone(), command: step.command.clone(), output, code });
                    false
                }
            };
            for u in &users[i] {
                if !ok {
                    blocked[*u] = true;
                }
                waiting[*u] -= 1;
                if waiting[*u] == 0 {
                    if blocked[*u] {
                        // Propagate the failure without running it.
                        let mut stack = vec![*u];
                        while let Some(x) = stack.pop() {
                            out.skipped += 1;
                            for y in &users[x] {
                                blocked[*y] = true;
                                waiting[*y] -= 1;
                                if waiting[*y] == 0 {
                                    stack.push(*y);
                                }
                            }
                        }
                    } else {
                        ready.push(*u);
                    }
                }
            }
        }
    });
    // Steps never started because the build stopped early.
    let started = out.ran + out.up_to_date + out.failed + out.skipped;
    if !opts.dry_run && out.failed > 0 && started < total {
        out.skipped += total - started;
    }
    drop(tx);
    out.state_changed = out.ran > 0 || out.failed > 0 || hasher.dirty.load(std::sync::atomic::Ordering::Relaxed);
    state.files = std::mem::take(&mut *hasher.cache_mut());
    match error {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// Deletes the outputs (and depfiles) of the selected steps and forgets them.
pub fn clean(root: &Path, graph: &Graph, selected: &[usize], state: &mut State) -> usize {
    let mut n = 0;
    for i in selected {
        let s = &graph.steps[*i];
        for p in s.outputs.iter().chain(s.depfile.iter()) {
            if std::fs::remove_file(root.join(p)).is_ok() {
                n += 1;
            }
            state.files.remove(&key_path(p));
        }
        if let Some(o) = s.outputs.first() {
            state.steps.remove(&key_path(o));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_split_off_command_lines() {
        assert_eq!(split_program("gcc -c a.c"), ("gcc".to_string(), " -c a.c"));
        assert_eq!(split_program("\"C:/Program Files/x.exe\" -v"), ("C:/Program Files/x.exe".to_string(), " -v"));
        assert_eq!(split_program("  tool"), ("tool".to_string(), ""));
        #[cfg(windows)]
        {
            assert!(needs_shell("echo hi"));
            assert!(needs_shell("gcc a.c > log"));
            assert!(needs_shell("a && b"));
            assert!(!needs_shell("arm-none-eabi-gcc -O2 -c a.c -o a.o"));
        }
    }
}
