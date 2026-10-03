//! Makefile-style dependency files, as `gcc -MD`, `clang -MD` and many other tools write them:
//! `target: dep1 dep2 \` with line continuations and `\ ` for spaces in names.

use std::path::PathBuf;

/// The prerequisites listed in a depfile (targets left out, duplicates removed, order kept).
pub fn parse(text: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    // Join continued lines.
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    for line in joined.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The rule's colon is the first ':' followed by whitespace or the end (so `C:\x` survives).
        let b = line.as_bytes();
        let mut colon = None;
        for i in 0..b.len() {
            if b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ' || b[i + 1] == b'\t') {
                colon = Some(i);
                break;
            }
        }
        let Some(c) = colon else { continue };
        let mut word = String::new();
        let mut chars = line[c + 1..].chars().peekable();
        let mut flush = |w: &mut String| {
            if !w.is_empty() {
                let p = PathBuf::from(w.replace('\\', "/"));
                if !out.contains(&p) {
                    out.push(p);
                }
                w.clear();
            }
        };
        while let Some(ch) = chars.next() {
            match ch {
                '\\' if chars.peek() == Some(&' ') => {
                    word.push(' ');
                    chars.next();
                }
                '$' if chars.peek() == Some(&'$') => {
                    word.push('$');
                    chars.next();
                }
                ' ' | '\t' => flush(&mut word),
                _ => word.push(ch),
            }
        }
        flush(&mut word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcc_style_with_continuations_spaces_and_drive_letters() {
        let d = "build/a.o: src/a.c src/a.h \\\n  include/b.h C:\\sdk\\c.h \\\n src/with\\ space.h\nsrc/a.h:\n";
        let got = parse(d);
        assert_eq!(
            got,
            vec![
                PathBuf::from("src/a.c"),
                PathBuf::from("src/a.h"),
                PathBuf::from("include/b.h"),
                PathBuf::from("C:/sdk/c.h"),
                PathBuf::from("src/with space.h")
            ]
        );
        // Windows line endings, `$$`, and a target with a drive letter.
        assert_eq!(parse("C:/out/a.o: a$$b.c \\\r\n b.h\r\n"), vec![PathBuf::from("a$b.c"), PathBuf::from("b.h")]);
        assert!(parse("").is_empty());
    }
}
