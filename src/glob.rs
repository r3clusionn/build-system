//! Glob patterns for inputs: `*` and `?` within a path component, `**` for any number of
//! directories, `[abc]` character classes. Matching is on `/`-separated relative paths.

use std::path::{Path, PathBuf};

/// Matches one path component against a component pattern.
fn component(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star_p, mut star_s) = (usize::MAX, 0);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star_p = pi;
                    star_s = si;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some(end) = p[pi + 1..].iter().position(|c| *c == b']') {
                        let class = &p[pi + 1..pi + 1 + end];
                        let (neg, class) = match class.first() {
                            Some(b'!') => (true, &class[1..]),
                            _ => (false, class),
                        };
                        let mut hit = false;
                        let mut i = 0;
                        while i < class.len() {
                            if i + 2 < class.len() && class[i + 1] == b'-' {
                                hit |= (class[i]..=class[i + 2]).contains(&s[si]);
                                i += 3;
                            } else {
                                hit |= class[i] == s[si];
                                i += 1;
                            }
                        }
                        if hit != neg {
                            pi += end + 2;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == b'[' {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        if star_p == usize::MAX {
            return false;
        }
        pi = star_p + 1;
        star_s += 1;
        si = star_s;
    }
    p[pi..].iter().all(|c| *c == b'*')
}

/// Whether a relative path (with `/` separators) matches a pattern.
pub fn matches(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let s: Vec<&str> = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    fn go(p: &[&str], s: &[&str]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(&"**") => (0..=s.len()).any(|k| go(&p[1..], &s[k..])),
            Some(c) => !s.is_empty() && component(c.as_bytes(), s[0].as_bytes()) && go(&p[1..], &s[1..]),
        }
    }
    go(&p, &s)
}

pub fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Files under `root` matching `pattern` (relative, `/`-separated), sorted. Directories named
/// in `skip` (e.g. the build output folder) are not entered.
pub fn expand(root: &Path, pattern: &str, skip: &[&str]) -> Vec<PathBuf> {
    // Walk only below the pattern's literal prefix.
    let parts: Vec<&str> = pattern.split('/').collect();
    let lit: Vec<&str> = parts.iter().take_while(|c| !has_magic(c)).copied().collect();
    if lit.len() == parts.len() {
        let p = root.join(pattern);
        return if p.is_file() { vec![PathBuf::from(pattern)] } else { Vec::new() };
    }
    let base = lit.join("/");
    let mut out = Vec::new();
    let mut stack = vec![root.join(&base)];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !skip.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push(path);
                }
            } else if matches(pattern, &rel) {
                out.push(PathBuf::from(rel));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_and_stars() {
        assert!(matches("src/*.c", "src/a.c"));
        assert!(!matches("src/*.c", "src/sub/a.c"));
        assert!(matches("src/**/*.c", "src/a.c"));
        assert!(matches("src/**/*.c", "src/x/y/a.c"));
        assert!(matches("**", "a/b/c"));
        assert!(matches("a?c", "abc") && !matches("a?c", "ac"));
        assert!(matches("*.[ch]", "x.h") && !matches("*.[ch]", "x.o"));
        assert!(matches("[!a]*", "bcd") && !matches("[!a]*", "abc"));
        assert!(matches("f[0-9].txt", "f7.txt") && !matches("f[0-9].txt", "fx.txt"));
        assert!(matches("./src/*.c", "src/a.c"));
        assert!(matches("*a*b*", "xxaxxbxx") && !matches("*a*b*", "xxbxxaxx"));
    }

    #[test]
    fn expansion_walks_from_the_literal_prefix_and_skips_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        for f in ["src/a.c", "src/b.h", "src/x/c.c", "build/src/z.c", "other/q.c"] {
            let p = d.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        let got = expand(d.path(), "src/**/*.c", &["build"]);
        assert_eq!(got, vec![PathBuf::from("src/a.c"), PathBuf::from("src/x/c.c")]);
        assert_eq!(expand(d.path(), "**/*.c", &["build"]).len(), 3);
        assert_eq!(expand(d.path(), "src/b.h", &[]), vec![PathBuf::from("src/b.h")]);
        assert!(expand(d.path(), "missing.c", &[]).is_empty());
    }
}
