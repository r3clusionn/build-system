//! End-to-end builds with the real `kiln` binary and `examples/tool.rs` as the compiler. Each
//! test counts the tool's runs through its log.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn tool() -> String {
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let p = deps.parent().unwrap().join("examples").join(if cfg!(windows) { "tool.exe" } else { "tool" });
    assert!(p.is_file(), "build the example: {}", p.display());
    p.to_string_lossy().replace('\\', "/")
}

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new(toml: &str, files: &[(&str, &str)]) -> Project {
        let dir = tempfile::tempdir().unwrap();
        let p = Project { dir };
        p.write("kiln.toml", &toml.replace("TOOL", &tool()));
        for (f, c) in files {
            p.write(f, c);
        }
        p
    }

    fn path(&self, f: &str) -> PathBuf {
        self.dir.path().join(f)
    }

    fn write(&self, f: &str, c: &str) {
        let p = self.path(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    fn read(&self, f: &str) -> String {
        std::fs::read_to_string(self.path(f)).unwrap()
    }

    /// Runs kiln; returns (success, stdout, tool runs during this build).
    fn kiln(&self, args: &[&str]) -> (bool, String, Vec<String>) {
        let log = self.path("tool.log");
        let before = std::fs::read_to_string(&log).unwrap_or_default().lines().count();
        let out = Command::new(env!("CARGO_BIN_EXE_kiln"))
            .args(args)
            .current_dir(self.dir.path())
            .env("KILN_LOG", &log)
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let runs: Vec<String> =
            std::fs::read_to_string(&log).unwrap_or_default().lines().skip(before).map(String::from).collect();
        (out.status.success(), text, runs)
    }

    fn touch(&self, f: &str) {
        let file = std::fs::File::options().write(true).open(self.path(f)).unwrap();
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(60)).unwrap();
    }
}

const C_PROJECT: &str = r#"
[profile.debug]
opt = "debug"
[profile.release]
opt = "release"

[[task]]
name = "compile"
foreach = "src/*.c"
output = "build/{profile}/{stem}.o"
depfile = "build/{profile}/{stem}.d"
command = "TOOL cc {out} {depfile} {in}"

[[task]]
name = "link"
inputs = ["@compile"]
output = "build/{profile}/app.txt"
command = "TOOL cat {out} {in}"
default = true
"#;

fn c_project() -> Project {
    Project::new(
        C_PROJECT,
        &[
            ("src/a.c", "#include \"common.h\"\na body\n"),
            ("src/b.c", "b body\n"),
            ("src/c.c", "#include \"common.h\"\nc body\n"),
            ("src/common.h", "common v1\n"),
        ],
    )
}

#[test]
fn a_second_build_does_nothing_and_touching_a_file_does_not_rebuild() {
    let p = c_project();
    let (ok, out, runs) = p.kiln(&[]);
    assert!(ok, "{out}");
    assert_eq!(runs.len(), 4, "3 compiles and a link");
    assert_eq!(p.read("build/debug/app.txt"), "common v1\na body\nb body\ncommon v1\nc body\n");
    let (ok, out, runs) = p.kiln(&[]);
    assert!(ok && runs.is_empty(), "{out}");
    assert!(out.contains("up to date"));
    p.touch("src/a.c");
    p.touch("src/common.h");
    let (_, _, runs) = p.kiln(&[]);
    assert!(runs.is_empty(), "same content, new times: nothing runs ({runs:?})");
}

#[test]
fn a_changed_source_rebuilds_only_its_step_and_the_link() {
    let p = c_project();
    p.kiln(&[]);
    p.write("src/b.c", "b body v2\n");
    let (_, _, runs) = p.kiln(&[]);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs[0].contains("src/b.c") && runs[1].starts_with("cat"));
    assert!(p.read("build/debug/app.txt").contains("b body v2"));
}

#[test]
fn headers_found_in_depfiles_trigger_rebuilds() {
    let p = c_project();
    p.kiln(&[]);
    // common.h is not declared anywhere; the depfiles of a.c and c.c name it.
    p.write("src/common.h", "common v2\n");
    let (_, _, runs) = p.kiln(&[]);
    let compiled: Vec<&String> = runs.iter().filter(|r| r.starts_with("cc")).collect();
    assert_eq!(compiled.len(), 2, "{runs:?}");
    assert!(compiled.iter().all(|r| !r.contains("b.c")));
    assert!(p.read("build/debug/app.txt").matches("common v2").count() == 2);
}

#[test]
fn early_cutoff_stops_at_an_unchanged_output() {
    let p = Project::new(
        r#"
[[task]]
name = "strip"
foreach = "src/*.txt"
output = "build/{stem}.stripped"
command = "TOOL strip {out} {in}"
[[task]]
name = "join"
inputs = ["@strip"]
output = "build/all.txt"
command = "TOOL cat {out} {in}"
"#,
        &[("src/x.txt", "# comment\nkeep\n"), ("src/y.txt", "other\n")],
    );
    p.kiln(&[]);
    // Only the comment changes: strip runs, writes the same bytes, and join does not run.
    p.write("src/x.txt", "# a different comment\nkeep\n");
    let (_, out, runs) = p.kiln(&[]);
    assert_eq!(runs.len(), 1, "{runs:?}\n{out}");
    assert!(runs[0].starts_with("strip"));
    p.write("src/x.txt", "# a different comment\nkeep, changed\n");
    let (_, _, runs) = p.kiln(&[]);
    assert_eq!(runs.len(), 2);
}

#[test]
fn profiles_build_apart_and_switching_back_is_free() {
    let p = c_project();
    p.kiln(&["--profile", "release"]);
    let (_, _, runs) = p.kiln(&["--profile", "debug"]);
    assert_eq!(runs.len(), 4);
    let (_, _, runs) = p.kiln(&["--profile", "release"]);
    assert!(runs.is_empty());
    assert!(p.path("build/release/app.txt").is_file() && p.path("build/debug/app.txt").is_file());
    let (ok, out, _) = p.kiln(&["--profile", "fast"]);
    assert!(!ok && out.contains("no profile `fast`"));
}

#[test]
fn modified_or_deleted_outputs_are_rebuilt() {
    let p = c_project();
    p.kiln(&[]);
    p.write("build/debug/b.o", "hand edited\n");
    std::fs::remove_file(p.path("build/debug/c.o")).unwrap();
    let (_, out, runs) = p.kiln(&["explain"]);
    assert!(runs.is_empty(), "explain runs nothing");
    assert!(out.contains("compile src/b.c: build/debug/b.o was modified"), "{out}");
    assert!(out.contains("compile src/c.c: build/debug/c.o is missing"), "{out}");
    assert!(out.contains("link build/debug/app.txt: a step it depends on would run"), "{out}");
    let (_, _, runs) = p.kiln(&[]);
    // b.o and c.o are rebuilt with the same content as before, so the link is up to date.
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(p.read("build/debug/b.o"), "b body\n");
}

#[test]
fn failures_stop_dependents_and_keep_going_builds_the_rest() {
    let p = Project::new(
        r#"
[[task]]
name = "good"
foreach = "src/*.txt"
output = "build/{stem}.out"
command = "TOOL cat {out} {in}"
[[task]]
name = "bad"
inputs = ["src/a.txt"]
output = "build/bad.out"
command = "TOOL fail"
[[task]]
name = "after_bad"
inputs = ["build/bad.out"]
output = "build/final.out"
command = "TOOL cat {out} {in}"
"#,
        &[("src/a.txt", "a\n"), ("src/b.txt", "b\n"), ("src/c.txt", "c\n")],
    );
    let (ok, out, _) = p.kiln(&["-k", "-j", "1"]);
    assert!(!ok);
    assert!(out.contains("FAILED: bad build/bad.out (exit 3)") && out.contains("failing on purpose"), "{out}");
    assert!(out.contains("1 failed") && out.contains("1 not built"), "{out}");
    assert!(p.path("build/a.out").is_file() && p.path("build/c.out").is_file());
    assert!(!p.path("build/final.out").exists());
    // Fixed: the failed step and the one it blocked run; the good ones stay built.
    let t = p.read("kiln.toml").replace(&format!("{} fail", tool()), &format!("{} cat {{out}} {{in}}", tool()));
    p.write("kiln.toml", &t);
    let (ok, out, runs) = p.kiln(&[]);
    assert!(ok, "{out}");
    assert_eq!(runs.len(), 2, "{runs:?}");
}

#[test]
fn independent_steps_run_in_parallel() {
    let mut toml = String::new();
    for i in 0..8 {
        toml.push_str(&format!("[[task]]\nname = \"s{i}\"\noutput = \"build/{i}.out\"\ncommand = \"TOOL sleep 250 {{out}}\"\n"));
    }
    let p = Project::new(&toml, &[]);
    let t = Instant::now();
    let (ok, _, runs) = p.kiln(&["-j", "8"]);
    let parallel = t.elapsed();
    assert!(ok && runs.len() == 8);
    p.kiln(&["clean"]);
    let t = Instant::now();
    p.kiln(&["-j", "1"]);
    let serial = t.elapsed();
    assert!(serial >= Duration::from_millis(2000), "{serial:?}");
    assert!(parallel < serial / 3, "-j 8 took {parallel:?}, -j 1 {serial:?}");
}

#[test]
fn mistakes_in_the_build_file_are_reported() {
    let p = Project::new(
        "[[task]]\nname = \"a\"\ninputs = [\"missing.txt\"]\noutput = \"o\"\ncommand = \"TOOL cat {out} {in}\"\n",
        &[],
    );
    let (ok, out, _) = p.kiln(&[]);
    assert!(!ok && out.contains("needs `missing.txt`, which does not exist"), "{out}");
    let p = Project::new(
        "[[task]]\nname = \"a\"\ninputs = [\"y\"]\noutput = \"x\"\ncommand = \"TOOL cat {out} {in}\"\n[[task]]\nname = \"b\"\ninputs = [\"x\"]\noutput = \"y\"\ncommand = \"TOOL cat {out} {in}\"\n",
        &[],
    );
    let (ok, out, _) = p.kiln(&[]);
    assert!(!ok && out.contains("dependency cycle"), "{out}");
    let p = Project::new("[[task]]\nname = \"a\"\noutput = \"x\"\ncommand = \"{nope}\"\n", &[]);
    let (ok, out, _) = p.kiln(&[]);
    assert!(!ok && out.contains("unknown variable `{nope}`"), "{out}");
}

#[test]
fn targets_select_part_of_the_graph_and_clean_removes_outputs() {
    let p = c_project();
    let (_, _, runs) = p.kiln(&["build/debug/b.o"]);
    assert_eq!(runs.len(), 1);
    let (_, out, _) = p.kiln(&["targets"]);
    assert!(out.contains("compile: 3 steps") && out.contains("link (default): 1 step"), "{out}");
    let (_, out, _) = p.kiln(&["graph"]);
    assert!(out.starts_with("digraph") && out.matches("->").count() == 3);
    p.kiln(&[]);
    let (_, out, _) = p.kiln(&["clean"]);
    assert!(out.contains("removed 7 files"), "{out}"); // 3 objects, 3 depfiles, the app
    assert!(!Path::new(&p.path("build/debug/app.txt")).exists());
}
