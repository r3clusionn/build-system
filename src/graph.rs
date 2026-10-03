//! The step graph: a step depends on the steps that produce its inputs and on every step of the
//! tasks it runs `after`. Duplicate outputs and cycles are errors.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::config::Step;

pub struct Graph {
    pub steps: Vec<Step>,
    /// For each step, the steps it waits for.
    pub deps: Vec<Vec<usize>>,
    pub producer: HashMap<PathBuf, usize>,
}

pub fn norm(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for c in s.split('/') {
        match c {
            "" | "." => {}
            ".." if parts.last().is_some_and(|l| *l != "..") => {
                parts.pop();
            }
            _ => parts.push(c),
        }
    }
    PathBuf::from(parts.join("/"))
}

impl Graph {
    pub fn new(mut steps: Vec<Step>) -> Result<Graph, String> {
        for s in &mut steps {
            s.inputs = s.inputs.iter().map(|p| norm(p)).collect();
            s.outputs = s.outputs.iter().map(|p| norm(p)).collect();
            s.depfile = s.depfile.as_ref().map(|p| norm(p));
        }
        let mut producer = HashMap::new();
        for (i, s) in steps.iter().enumerate() {
            for o in &s.outputs {
                if let Some(j) = producer.insert(o.clone(), i) {
                    return Err(format!(
                        "`{}` is an output of both `{}` and `{}`",
                        o.display(),
                        steps[j].description,
                        s.description
                    ));
                }
            }
        }
        let mut by_task: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, s) in steps.iter().enumerate() {
            by_task.entry(s.task.as_str()).or_default().push(i);
        }
        let mut deps = Vec::with_capacity(steps.len());
        for (i, s) in steps.iter().enumerate() {
            let mut d = BTreeSet::new();
            for inp in &s.inputs {
                if let Some(j) = producer.get(inp) {
                    if *j == i {
                        return Err(format!("`{}` reads its own output `{}`", s.description, inp.display()));
                    }
                    d.insert(*j);
                }
            }
            for t in &s.after {
                let list = by_task.get(t.as_str()).ok_or_else(|| format!("`{}` runs after unknown task `{t}`", s.description))?;
                d.extend(list.iter().copied().filter(|j| *j != i));
            }
            deps.push(d.into_iter().collect());
        }
        let g = Graph { steps, deps, producer };
        g.check_cycles()?;
        Ok(g)
    }

    fn check_cycles(&self) -> Result<(), String> {
        let n = self.steps.len();
        let mut indeg: Vec<usize> = self.deps.iter().map(|d| d.len()).collect();
        let mut users = vec![Vec::new(); n];
        for (i, d) in self.deps.iter().enumerate() {
            for j in d {
                users[*j].push(i);
            }
        }
        let mut stack: Vec<usize> = (0..n).filter(|i| indeg[*i] == 0).collect();
        let mut seen = 0;
        while let Some(i) = stack.pop() {
            seen += 1;
            for u in &users[i] {
                indeg[*u] -= 1;
                if indeg[*u] == 0 {
                    stack.push(*u);
                }
            }
        }
        if seen < n {
            let stuck: Vec<&str> = (0..n).filter(|i| indeg[*i] > 0).take(4).map(|i| self.steps[i].description.as_str()).collect();
            return Err(format!("dependency cycle among: {}", stuck.join(", ")));
        }
        Ok(())
    }

    /// The steps needed for the targets (task names or output paths) and everything they depend
    /// on. No targets: the given default tasks, or every step.
    pub fn select(&self, targets: &[String], defaults: &[String]) -> Result<Vec<usize>, String> {
        let wanted: Vec<String> = if targets.is_empty() { defaults.to_vec() } else { targets.to_vec() };
        let mut roots = Vec::new();
        if wanted.is_empty() {
            roots.extend(0..self.steps.len());
        }
        for t in &wanted {
            let by_task: Vec<usize> = (0..self.steps.len()).filter(|i| self.steps[*i].task == *t).collect();
            if !by_task.is_empty() {
                roots.extend(by_task);
            } else if let Some(i) = self.producer.get(&norm(Path::new(t))) {
                roots.push(*i);
            } else {
                return Err(format!("no task or output called `{t}`"));
            }
        }
        let mut keep = vec![false; self.steps.len()];
        let mut stack = roots;
        while let Some(i) = stack.pop() {
            if !keep[i] {
                keep[i] = true;
                stack.extend(self.deps[i].iter().copied());
            }
        }
        Ok((0..self.steps.len()).filter(|i| keep[*i]).collect())
    }

    /// Graphviz DOT of the selected steps.
    pub fn dot(&self, selected: &[usize]) -> String {
        let mut out = String::from("digraph kiln {\n  rankdir=LR;\n  node [shape=box, fontname=\"monospace\"];\n");
        for i in selected {
            out.push_str(&format!("  s{i} [label=\"{}\"];\n", self.steps[*i].description.replace('"', "\\\"")));
            for d in &self.deps[*i] {
                out.push_str(&format!("  s{d} -> s{i};\n"));
            }
        }
        out.push_str("}\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(task: &str, inputs: &[&str], outputs: &[&str]) -> Step {
        Step {
            task: task.into(),
            description: format!("{task} {}", outputs.join(",")),
            command: String::new(),
            inputs: inputs.iter().map(PathBuf::from).collect(),
            outputs: outputs.iter().map(PathBuf::from).collect(),
            depfile: None,
            after: Vec::new(),
        }
    }

    #[test]
    fn producers_become_dependencies_and_targets_pull_in_their_closure() {
        let g = Graph::new(vec![
            step("cc", &["a.c"], &["a.o"]),
            step("cc", &["b.c"], &["b.o"]),
            step("link", &["a.o", "./b.o"], &["app"]),
            step("doc", &["readme"], &["doc.html"]),
        ])
        .unwrap();
        assert_eq!(g.deps[2], vec![0, 1]);
        assert_eq!(g.select(&["app".into()], &[]).unwrap(), vec![0, 1, 2]);
        assert_eq!(g.select(&["cc".into()], &[]).unwrap(), vec![0, 1]);
        assert_eq!(g.select(&[], &["doc".into()]).unwrap(), vec![3]);
        assert_eq!(g.select(&[], &[]).unwrap().len(), 4);
        assert!(g.select(&["nope".into()], &[]).is_err());
        assert!(g.dot(&[0, 2]).contains("s0 -> s2"));
    }

    #[test]
    fn duplicate_outputs_and_cycles_are_errors() {
        assert!(Graph::new(vec![step("a", &[], &["x"]), step("b", &[], &["x"])]).err().unwrap().contains("output of both"));
        let e = Graph::new(vec![step("a", &["y"], &["x"]), step("b", &["x"], &["y"])]).err().unwrap();
        assert!(e.contains("cycle"), "{e}");
        assert!(Graph::new(vec![step("a", &["x"], &["x"])]).err().unwrap().contains("its own output"));
    }

    #[test]
    fn after_orders_without_reading() {
        let mut s = step("b", &[], &["y"]);
        s.after = vec!["a".into()];
        let g = Graph::new(vec![step("a", &[], &["x"]), s]).unwrap();
        assert_eq!(g.deps[1], vec![0]);
        assert_eq!(norm(Path::new("./a/../b//c")), PathBuf::from("b/c"));
    }
}
