//! A tiny portable "compiler" for kiln's tests and demo. Every run appends a line to the file
//! named by `KILN_LOG` (if set), so a test can count what ran.
//!
//!     tool cat OUT IN...        concatenate
//!     tool strip OUT IN         copy without lines starting with '#'
//!     tool cc OUT DEPFILE IN    inline `#include "file"` lines (recursively), write a depfile
//!     tool sleep MS OUT         wait, then write OUT
//!     tool fail                 exit with status 3

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn log(line: &str) {
    if let Ok(p) = std::env::var("KILN_LOG") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        writeln!(f, "{line}").unwrap();
    }
}

fn include(path: &Path, out: &mut String, deps: &mut Vec<PathBuf>, depth: usize) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#include \"") {
            let name = rest.trim_end_matches('"');
            let p = path.parent().unwrap_or(Path::new("")).join(name);
            if !deps.contains(&p) {
                deps.push(p.clone());
            }
            if depth < 16 {
                include(&p, out, deps, depth + 1);
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    log(&a.join(" "));
    match a.first().map(|s| s.as_str()) {
        Some("cat") => {
            let mut s = Vec::new();
            for i in &a[2..] {
                s.extend(std::fs::read(i).unwrap_or_else(|e| panic!("{i}: {e}")));
            }
            std::fs::write(&a[1], s).unwrap();
        }
        Some("strip") => {
            let t = std::fs::read_to_string(&a[2]).unwrap();
            let kept: String = t.lines().filter(|l| !l.starts_with('#')).map(|l| format!("{l}\n")).collect();
            std::fs::write(&a[1], kept).unwrap();
        }
        Some("cc") => {
            let src = PathBuf::from(&a[3]);
            let mut out = String::new();
            let mut deps = vec![src.clone()];
            include(&src, &mut out, &mut deps, 0);
            std::fs::write(&a[1], out).unwrap();
            let mut d = format!("{}:", a[1]);
            for p in &deps {
                let _ = write!(d, " \\\n  {}", p.to_string_lossy().replace('\\', "/").replace(' ', "\\ "));
            }
            d.push('\n');
            std::fs::write(&a[2], d).unwrap();
        }
        Some("sleep") => {
            std::thread::sleep(std::time::Duration::from_millis(a[1].parse().unwrap()));
            std::fs::write(&a[2], "slept").unwrap();
        }
        Some("fail") => {
            eprintln!("tool: failing on purpose");
            std::process::exit(3);
        }
        _ => {
            eprintln!("usage: tool cat|strip|cc|sleep|fail ...");
            std::process::exit(2);
        }
    }
}
