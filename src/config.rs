//! The build file (`kiln.toml`): variables, profiles and tasks, and their expansion into concrete
//! steps (one command with its inputs and outputs).
//!
//! ```toml
//! [vars]
//! cc = "gcc"
//! cflags = "-Wall"
//!
//! [profile.debug]
//! cflags = "-Wall -O0 -g"
//!
//! [profile.release]
//! cflags = "-Wall -O2"
//!
//! [[task]]
//! name = "compile"
//! foreach = "src/**/*.c"                    # one step per matching file
//! output = "build/{profile}/obj/{stem}.o"
//! depfile = "build/{profile}/obj/{stem}.d"  # headers the compiler reports
//! command = "{cc} {cflags} -MD -MF {depfile} -c {in} -o {out}"
//!
//! [[task]]
//! name = "link"
//! inputs = ["@compile"]                     # every output of another task
//! output = "build/{profile}/app"
//! command = "{cc} {in} -o {out}"
//! default = true
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::glob;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BuildFile {
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub profile: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default, rename = "task")]
    pub tasks: Vec<TaskDef>,
    /// Folders never searched for inputs (default: `build`).
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TaskDef {
    pub name: String,
    /// A glob: one step per matching file, which is that step's `{in}`.
    pub foreach: Option<String>,
    /// Files, globs, or `@task` for every output of another task.
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    pub command: String,
    /// A Makefile-style dependency file the command writes (`-MD`): extra inputs found at build time.
    pub depfile: Option<String>,
    /// Tasks that must finish first without being inputs (order only).
    #[serde(default)]
    pub after: Vec<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
}

/// One command to run, with everything it reads and writes.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub task: String,
    pub description: String,
    pub command: String,
    pub inputs: Vec<PathBuf>,
    pub outputs: Vec<PathBuf>,
    pub depfile: Option<PathBuf>,
    /// Tasks this step runs after without reading their outputs.
    pub after: Vec<String>,
}

pub fn parse(text: &str) -> Result<BuildFile, String> {
    let f: BuildFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = std::collections::HashSet::new();
    for t in &f.tasks {
        if !seen.insert(&t.name) {
            return Err(format!("task `{}` is defined twice", t.name));
        }
        if t.output.is_none() && t.outputs.is_empty() {
            return Err(format!("task `{}` has no output", t.name));
        }
    }
    Ok(f)
}

/// Replaces `{name}` with variables; `{{` and `}}` are literal braces. Unknown names are errors.
pub fn substitute(s: &str, vars: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' if b.get(i + 1) == Some(&b'{') => {
                out.push('{');
                i += 2;
            }
            b'}' if b.get(i + 1) == Some(&b'}') => {
                out.push('}');
                i += 2;
            }
            b'{' => {
                let end = s[i..].find('}').ok_or_else(|| format!("unclosed `{{` in `{s}`"))? + i;
                let name = &s[i + 1..end];
                out.push_str(&vars(name).ok_or_else(|| format!("unknown variable `{{{name}}}` in `{s}`"))?);
                i = end + 1;
            }
            _ => {
                let ch = s[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Ok(out)
}

/// Quotes a path for the shell when it has spaces.
fn quote(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s
    }
}

fn rel(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Expands every task into steps for a profile.
pub fn expand(file: &BuildFile, root: &Path, profile: &str) -> Result<Vec<Step>, String> {
    if !file.profile.is_empty() && !file.profile.contains_key(profile) {
        let names: Vec<&String> = file.profile.keys().collect();
        return Err(format!(
            "no profile `{profile}` (profiles: {})",
            names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let mut vars = file.vars.clone();
    if let Some(p) = file.profile.get(profile) {
        vars.extend(p.clone());
    }
    vars.insert("profile".into(), profile.to_string());
    let skip: Vec<&str> = if file.ignore.is_empty() { vec!["build"] } else { file.ignore.iter().map(|s| s.as_str()).collect() };

    // Variables may refer to each other ({cflags} = "{base} -O2"); resolve a few levels deep.
    for _ in 0..4 {
        let snapshot = vars.clone();
        for v in vars.values_mut() {
            if v.contains('{') {
                if let Ok(s) = substitute(v, &|n| snapshot.get(n).cloned()) {
                    *v = s;
                }
            }
        }
    }

    let mut steps: Vec<Step> = Vec::new();
    let mut outputs_of: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    // Tasks are expanded in dependency order of their `@task` references.
    let order = task_order(&file.tasks)?;
    for ti in order {
        let t = &file.tasks[ti];
        let mut base_inputs: Vec<PathBuf> = Vec::new();
        for i in &t.inputs {
            let i = substitute(i, &|n| vars.get(n).cloned())?;
            if let Some(task) = i.strip_prefix('@') {
                base_inputs.extend(outputs_of.get(task).cloned().ok_or_else(|| format!("task `{}`: no task `{task}`", t.name))?);
            } else if glob::has_magic(&i) {
                base_inputs.extend(glob::expand(root, &i, &skip));
            } else {
                base_inputs.push(PathBuf::from(i));
            }
        }
        let each: Vec<Option<PathBuf>> = match &t.foreach {
            Some(g) => {
                let g = substitute(g, &|n| vars.get(n).cloned())?;
                let files = if let Some(task) = g.strip_prefix('@') {
                    outputs_of.get(task).cloned().ok_or_else(|| format!("task `{}`: no task `{task}`", t.name))?
                } else {
                    glob::expand(root, &g, &skip)
                };
                files.into_iter().map(Some).collect()
            }
            None => vec![None],
        };
        let mut produced = Vec::new();
        for item in each {
            let mut inputs = Vec::new();
            if let Some(f) = &item {
                inputs.push(f.clone());
            }
            inputs.extend(base_inputs.iter().cloned());
            let stem = item.as_ref().and_then(|f| f.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let fname = item.as_ref().and_then(|f| f.file_name()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = item.as_ref().and_then(|f| f.parent()).map(rel).unwrap_or_default();
            let path_vars = |n: &str| -> Option<String> {
                match n {
                    "stem" => Some(stem.clone()),
                    "file" => Some(fname.clone()),
                    "dir" => Some(dir.clone()),
                    "task" => Some(t.name.clone()),
                    _ => vars.get(n).cloned(),
                }
            };
            let mut outs = Vec::new();
            for o in t.output.iter().chain(t.outputs.iter()) {
                outs.push(PathBuf::from(substitute(o, &path_vars)?));
            }
            let depfile = t.depfile.as_ref().map(|d| substitute(d, &path_vars).map(PathBuf::from)).transpose()?;
            let in_list = inputs.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
            let out_list = outs.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
            let dep_s = depfile.as_ref().map(|d| quote(d)).unwrap_or_default();
            let all = |n: &str| -> Option<String> {
                match n {
                    "in" => Some(in_list.clone()),
                    "out" => Some(out_list.clone()),
                    "depfile" => Some(dep_s.clone()),
                    _ => path_vars(n),
                }
            };
            let command = substitute(&t.command, &all)?;
            let description = match &t.description {
                Some(d) => substitute(d, &all)?,
                None => match &item {
                    Some(f) => format!("{} {}", t.name, rel(f)),
                    None => format!("{} {}", t.name, outs.first().map(|o| rel(o)).unwrap_or_default()),
                },
            };
            produced.extend(outs.iter().cloned());
            steps.push(Step {
                task: t.name.clone(),
                description,
                command,
                inputs,
                outputs: outs,
                depfile,
                after: t.after.clone(),
            });
        }
        outputs_of.insert(t.name.clone(), produced);
    }
    Ok(steps)
}

/// Tasks in an order where every `@task` reference comes before its user.
fn task_order(tasks: &[TaskDef]) -> Result<Vec<usize>, String> {
    let index: BTreeMap<&str, usize> = tasks.iter().enumerate().map(|(i, t)| (t.name.as_str(), i)).collect();
    let refs = |t: &TaskDef| -> Vec<String> {
        t.inputs.iter().chain(t.foreach.iter()).filter_map(|i| i.strip_prefix('@').map(|s| s.to_string())).collect()
    };
    let mut state = vec![0u8; tasks.len()]; // 0 new, 1 visiting, 2 done
    let mut out = Vec::new();
    fn visit(
        i: usize,
        tasks: &[TaskDef],
        index: &BTreeMap<&str, usize>,
        refs: &dyn Fn(&TaskDef) -> Vec<String>,
        state: &mut Vec<u8>,
        out: &mut Vec<usize>,
    ) -> Result<(), String> {
        match state[i] {
            2 => return Ok(()),
            1 => return Err(format!("tasks refer to each other in a cycle through `{}`", tasks[i].name)),
            _ => {}
        }
        state[i] = 1;
        for r in refs(&tasks[i]) {
            let j = *index.get(r.as_str()).ok_or_else(|| format!("task `{}`: no task `{r}`", tasks[i].name))?;
            visit(j, tasks, index, refs, state, out)?;
        }
        state[i] = 2;
        out.push(i);
        Ok(())
    }
    for i in 0..tasks.len() {
        visit(i, tasks, &index, &refs, &mut state, &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_and_escapes() {
        let v = |n: &str| match n {
            "a" => Some("1".to_string()),
            "b" => Some("two".to_string()),
            _ => None,
        };
        assert_eq!(substitute("x{a}y{b}", &v).unwrap(), "x1ytwo");
        assert_eq!(substitute("{{a}} {a}", &v).unwrap(), "{a} 1");
        assert!(substitute("{nope}", &v).unwrap_err().contains("unknown variable"));
        assert!(substitute("{a", &v).unwrap_err().contains("unclosed"));
        assert_eq!(substitute("ä{a}ö", &v).unwrap(), "ä1ö");
    }

    #[test]
    fn tasks_expand_per_file_with_profiles_and_references() {
        let d = tempfile::tempdir().unwrap();
        for f in ["src/a.c", "src/b.c", "src/inc.h"] {
            let p = d.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        // The link task is listed first; `@compile` must still be expanded before it.
        let f = parse(
            r#"
            [vars]
            cc = "cc"
            opt = "-O0"
            cflags = "-Wall {opt}"
            [profile.debug]
            [profile.release]
            opt = "-O2"
            [[task]]
            name = "link"
            inputs = ["@compile"]
            output = "build/{profile}/app"
            command = "{cc} {in} -o {out}"
            [[task]]
            name = "compile"
            foreach = "src/*.c"
            inputs = ["src/inc.h"]
            output = "build/{profile}/{stem}.o"
            depfile = "build/{profile}/{stem}.d"
            command = "{cc} {cflags} -c {in} -o {out} -MF {depfile}"
            "#,
        )
        .unwrap();
        let s = expand(&f, d.path(), "release").unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].command, "cc -Wall -O2 -c src/a.c src/inc.h -o build/release/a.o -MF build/release/a.d");
        assert_eq!(s[0].description, "compile src/a.c");
        assert_eq!(s[2].command, "cc build/release/a.o build/release/b.o -o build/release/app");
        assert_eq!(expand(&f, d.path(), "debug").unwrap()[0].outputs, vec![PathBuf::from("build/debug/a.o")]);
        assert!(expand(&f, d.path(), "fast").unwrap_err().contains("no profile `fast`"));
    }

    #[test]
    fn bad_files_are_refused() {
        assert!(parse("[[task]]\nname='a'\ncommand='x'\n").unwrap_err().contains("no output"));
        assert!(parse("[[task]]\nname='a'\noutput='o'\ncommand='x'\n[[task]]\nname='a'\noutput='p'\ncommand='x'\n")
            .unwrap_err()
            .contains("twice"));
        assert!(parse("[[task]]\nname='a'\noutput='o'\ncommand='x'\nbogus=1\n").is_err());
        let cyc = parse("[[task]]\nname='a'\ninputs=['@b']\noutput='o'\ncommand='x'\n[[task]]\nname='b'\ninputs=['@a']\noutput='p'\ncommand='x'\n").unwrap();
        assert!(expand(&cyc, Path::new("."), "x").unwrap_err().contains("cycle"));
    }
}
