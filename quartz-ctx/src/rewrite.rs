//! Turning an agent's shell read into a navigation call.
//!
//! Agents search and read code with `grep` and `sed -n` through the shell far
//! more than with any code tool: over 14 days on this workspace, 29% of every
//! shell pipeline was a grep of code and 19% a `sed -n` line read, against a
//! handful of navigation calls. Instructions to prefer the tools compete with
//! a host prompt that tells the agent to use grep and sed, so asking does not
//! move it.
//!
//! A `PreToolUse` hook can replace the command before it runs. This module
//! decides when that is safe: only for the command shapes [`rewrite`] fully
//! understands, and only where the answer it gives instead holds everything
//! the original would have printed - the same matching lines or the same line
//! range - plus the item each line sits in. Anything else (an output filter,
//! an unknown flag, a redirection, a command substitution, a pipeline into
//! anything but `head` or `wc -l`) is left exactly as written.

/// A shell word: its text as written, and its value once quotes are removed.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word { raw: String, val: String },
    Op(&'static str),
}

/// Words and operators of one command line. `None` for anything this module
/// does not model: command substitution, redirection other than
/// `2>/dev/null`, subshells, background jobs, more than one line.
fn tokenize(cmd: &str) -> Option<Vec<Tok>> {
    let b: Vec<char> = cmd.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' => {
                i += 1;
                continue;
            }
            '\n' | '\r' | '<' | '>' | '(' | ')' | '`' => return None,
            '|' if b.get(i + 1) == Some(&'|') => {
                toks.push(Tok::Op("||"));
                i += 2;
                continue;
            }
            '|' => {
                toks.push(Tok::Op("|"));
                i += 1;
                continue;
            }
            '&' if b.get(i + 1) == Some(&'&') => {
                toks.push(Tok::Op("&&"));
                i += 2;
                continue;
            }
            '&' => return None,
            ';' => {
                toks.push(Tok::Op(";"));
                i += 1;
                continue;
            }
            '2' if b.get(i + 1) == Some(&'>') => {
                let rest: String = b[i..].iter().collect();
                if rest.starts_with("2>/dev/null") {
                    toks.push(Tok::Op("2>/dev/null"));
                    i += "2>/dev/null".len();
                    continue;
                }
                return None;
            }
            _ => {}
        }
        let mut raw = String::new();
        let mut val = String::new();
        while i < b.len() {
            let c = b[i];
            match c {
                ' ' | '\t' | '|' | '&' | ';' => break,
                '\n' | '\r' | '<' | '>' | '(' | ')' | '`' => return None,
                '$' if b.get(i + 1) == Some(&'(') => return None,
                '\'' => {
                    let j = i + 1 + b[i + 1..].iter().position(|&x| x == '\'')?;
                    raw.extend(&b[i..=j]);
                    val.extend(&b[i + 1..j]);
                    i = j + 1;
                }
                '"' => {
                    let mut j = i + 1;
                    raw.push('"');
                    while j < b.len() && b[j] != '"' {
                        match b[j] {
                            '\\' if matches!(b.get(j + 1), Some('"' | '\\' | '$' | '`')) => {
                                raw.push('\\');
                                raw.push(b[j + 1]);
                                val.push(b[j + 1]);
                                j += 2;
                            }
                            '`' => return None,
                            '$' if b.get(j + 1) == Some(&'(') => return None,
                            x => {
                                raw.push(x);
                                val.push(x);
                                j += 1;
                            }
                        }
                    }
                    if j >= b.len() {
                        return None;
                    }
                    raw.push('"');
                    i = j + 1;
                }
                '\\' => {
                    let n = *b.get(i + 1)?;
                    raw.push('\\');
                    raw.push(n);
                    val.push(n);
                    i += 2;
                }
                _ => {
                    raw.push(c);
                    val.push(c);
                    i += 1;
                }
            }
        }
        toks.push(Tok::Word { raw, val });
    }
    Some(toks)
}

/// The rewritten command, and what kind of read it replaced.
#[derive(Debug, Clone, PartialEq)]
pub struct Rewrite {
    pub command: String,
    /// `search` or `read`, one per rewritten segment.
    pub kinds: Vec<&'static str>,
}

/// The navigation command answering `cmd`, or `None` to run `cmd` as written.
///
/// `qx` is how to invoke quartz-ctx (an absolute path, quoted if needed), and
/// `manifest` the sources manifest to pass along, so the rewritten command
/// finds the same roots from any directory the agent has moved to.
pub fn rewrite(cmd: &str, qx: &str, manifest: Option<&str>) -> Option<Rewrite> {
    if cmd.contains("QX_RAW=1") {
        return None;
    }
    let toks = tokenize(cmd.trim())?;
    // Split into segments at `;` and `&&`, keeping the separators.
    let mut segments: Vec<(Vec<Tok>, Option<&'static str>)> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    for t in toks {
        match t {
            Tok::Op(sep @ (";" | "&&")) => segments.push((std::mem::take(&mut cur), Some(sep))),
            Tok::Op("||") => return None,
            t => cur.push(t),
        }
    }
    segments.push((cur, None));

    let head = match manifest {
        Some(m) => format!("{qx} nav --sources-from {} --hook", shell_quote(m)),
        None => format!("{qx} nav --hook"),
    };
    let mut out = String::new();
    let mut kinds = Vec::new();
    for (seg, sep) in &segments {
        if seg.is_empty() {
            if sep.is_some() {
                return None;
            }
            continue;
        }
        let words: Vec<&str> = seg
            .iter()
            .map(|t| match t {
                Tok::Word { val, .. } => val.as_str(),
                Tok::Op(o) => o,
            })
            .collect();
        let raw_text = seg
            .iter()
            .map(|t| match t {
                Tok::Word { raw, .. } => raw.clone(),
                Tok::Op(o) => o.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ");
        let kept = match words[0] {
            "cd" | "echo" | "printf" | "true" => true,
            w => seg.len() == 1 && is_assignment(w),
        };
        if kept {
            out.push_str(&raw_text);
        } else {
            // A read's exit status is not grep's: an `&&` after one would
            // change what runs next.
            if *sep == Some("&&") {
                return None;
            }
            let (text, kind) = match words[0] {
                "grep" | "egrep" => (grep_segment(seg, &head)?, "search"),
                "sed" => (sed_segment(seg, &head)?, "read"),
                _ => return None,
            };
            out.push_str(&text);
            kinds.push(kind);
        }
        if let Some(sep) = sep {
            out.push_str(if *sep == ";" { "; " } else { " && " });
        }
    }
    (!kinds.is_empty()).then(|| Rewrite { command: out.trim().to_string(), kinds })
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Single-quoted for the shell, unless it needs nothing.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Directories every search skips anyway, so excluding them changes nothing.
const SKIPPED: &[&str] = &["target", "node_modules", ".git", "dist", "build", "venv", ".venv", "__pycache__"];

/// `grep [flags] PATTERN PATH... [2>/dev/null] [| head -N | wc -l]`.
fn grep_segment(seg: &[Tok], head: &str) -> Option<String> {
    let mut extended = matches!(seg.first(), Some(Tok::Word { val, .. }) if val == "egrep");
    let (mut fixed, mut icase, mut word, mut files, mut count) = (false, false, false, false, false);
    let (mut before, mut after) = (0usize, 0usize);
    let mut globs: Vec<String> = Vec::new();
    let mut pattern: Option<(String, String)> = None; // (raw, value)
    let mut paths: Vec<String> = Vec::new();
    let mut limit: Option<usize> = None;
    let mut only_args = false;

    let mut i = 1;
    // The grep command itself: everything before the first operator.
    while i < seg.len() {
        let (raw, val) = match &seg[i] {
            Tok::Word { raw, val } => (raw.clone(), val.clone()),
            Tok::Op(_) => break,
        };
        i += 1;
        if only_args || !val.starts_with('-') || val == "-" {
            if val == "-" {
                return None; // stdin
            }
            if pattern.is_none() {
                pattern = Some((raw, val));
            } else {
                paths.push(raw);
            }
            continue;
        }
        if val == "--" {
            only_args = true;
            continue;
        }
        if let Some(long) = val.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            match name {
                "line-number" | "recursive" | "dereference-recursive" | "no-messages" | "with-filename"
                | "no-filename" => {}
                "color" | "colour" => {}
                "binary-files" if value.as_deref() == Some("without-match") => {}
                "ignore-case" => icase = true,
                "word-regexp" => word = true,
                "fixed-strings" => fixed = true,
                "extended-regexp" => extended = true,
                "files-with-matches" => files = true,
                "count" => count = true,
                "include" => globs.push(value?),
                "exclude-dir" => {
                    let v = value?;
                    let v = v.trim_matches(|c| c == '{' || c == '}');
                    if !v.split(',').all(|d| SKIPPED.contains(&d.trim())) {
                        return None;
                    }
                }
                "context" => {
                    let n = value?.parse().ok()?;
                    (before, after) = (n, n);
                }
                "after-context" => after = value?.parse().ok()?,
                "before-context" => before = value?.parse().ok()?,
                _ => return None,
            }
            continue;
        }
        // A cluster of short flags: -rn, -niE, -A5, -C 3, -e PATTERN.
        let flags: Vec<char> = val[1..].chars().collect();
        let mut k = 0;
        while k < flags.len() {
            match flags[k] {
                'n' | 'r' | 'R' | 'H' | 'h' | 'I' => {}
                's' => {}
                'i' => icase = true,
                'w' => word = true,
                'F' => fixed = true,
                'E' => extended = true,
                'G' => extended = false,
                'l' => files = true,
                'c' => count = true,
                f @ ('A' | 'B' | 'C') => {
                    let rest: String = flags[k + 1..].iter().collect();
                    let n: usize = if !rest.is_empty() {
                        rest.parse().ok()?
                    } else {
                        let Tok::Word { val, .. } = seg.get(i)? else { return None };
                        i += 1;
                        val.parse().ok()?
                    };
                    match f {
                        'A' => after = n,
                        'B' => before = n,
                        _ => (before, after) = (n, n),
                    }
                    break;
                }
                'e' => {
                    let rest: String = flags[k + 1..].iter().collect();
                    if pattern.is_some() {
                        return None; // several patterns
                    }
                    if !rest.is_empty() {
                        pattern = Some((shell_quote(&rest), rest));
                    } else {
                        let Tok::Word { raw, val } = seg.get(i)? else { return None };
                        pattern = Some((raw.clone(), val.clone()));
                        i += 1;
                    }
                    only_args = false;
                    break;
                }
                _ => return None,
            }
            k += 1;
        }
    }
    let (pattern_raw, pattern_val) = pattern?;
    if paths.is_empty() || pattern_val.contains('$') && !pattern_raw.starts_with('\'') {
        return None; // stdin, or a pattern the shell has yet to expand
    }
    // What follows: 2>/dev/null, then `| head [-N]` or `| wc -l`.
    let mut devnull = false;
    while i < seg.len() {
        match &seg[i] {
            Tok::Op("2>/dev/null") => {
                devnull = true;
                i += 1;
            }
            Tok::Op("|") => {
                i += 1;
                let words: Vec<&str> = seg[i..]
                    .iter()
                    .take_while(|t| !matches!(t, Tok::Op(_)))
                    .map(|t| match t {
                        Tok::Word { val, .. } => val.as_str(),
                        Tok::Op(o) => o,
                    })
                    .collect();
                i += words.len();
                match words.as_slice() {
                    ["head"] => limit = Some(limit.unwrap_or(10).min(10)),
                    ["head", n] | ["head", "-n", n] if head_count(n).is_some() => {
                        let n = head_count(n)?;
                        limit = Some(limit.map_or(n, |l| l.min(n)));
                    }
                    ["wc", "-l"] => count = true,
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    // Only what our regex reads the same way.
    let probe = crate::nav::SearchOpts {
        fixed,
        basic: !extended && !fixed,
        word,
        ignore_case: icase,
        ..Default::default()
    };
    crate::nav::check_pattern(&pattern_val, &probe).ok()?;

    let mut cmd = format!("{head} search");
    if !extended && !fixed {
        cmd.push_str(" --basic");
    }
    if fixed {
        cmd.push_str(" -F");
    }
    if icase {
        cmd.push_str(" -i");
    }
    if word {
        cmd.push_str(" -w");
    }
    if before == after && before > 0 {
        cmd.push_str(&format!(" -C {before}"));
    } else {
        if before > 0 {
            cmd.push_str(&format!(" -B {before}"));
        }
        if after > 0 {
            cmd.push_str(&format!(" -A {after}"));
        }
    }
    if count {
        cmd.push_str(" -c");
    } else if files {
        cmd.push_str(" -l");
    }
    if !globs.is_empty() {
        cmd.push_str(&format!(" --glob {}", shell_quote(&globs.join(","))));
    }
    // `| head -N` counts printed lines, context included.
    if let Some(n) = limit {
        cmd.push_str(&format!(" --head {n}"));
    }
    cmd.push_str(&format!(" -- {pattern_raw} {}", paths.join(" ")));
    if devnull {
        cmd.push_str(" 2>/dev/null");
    }
    Some(cmd)
}

/// `-20` or `20` as a line count.
fn head_count(s: &str) -> Option<usize> {
    s.strip_prefix('-').unwrap_or(s).parse().ok().filter(|n| *n > 0)
}

/// `sed -n 'A,Bp[;C,Dp]' FILE`, also `A,$p`, `Ap`, `-e` scripts, `-ne`.
fn sed_segment(seg: &[Tok], head: &str) -> Option<String> {
    let mut quiet = false;
    let mut scripts: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut i = 1;
    while i < seg.len() {
        let (raw, val) = match &seg[i] {
            Tok::Word { raw, val } => (raw.clone(), val.clone()),
            Tok::Op("2>/dev/null") => {
                i += 1;
                continue;
            }
            Tok::Op(_) => return None, // a pipe or anything after sed
        };
        i += 1;
        match val.as_str() {
            "-n" => quiet = true,
            "-ne" | "-en" => {
                quiet = true;
                let Tok::Word { val, .. } = seg.get(i)? else { return None };
                scripts.push(val.clone());
                i += 1;
            }
            "-e" => {
                let Tok::Word { val, .. } = seg.get(i)? else { return None };
                scripts.push(val.clone());
                i += 1;
            }
            v if v.starts_with('-') => return None,
            _ if scripts.is_empty() => scripts.push(val),
            _ => files.push(raw),
        }
    }
    if !quiet || files.len() != 1 || scripts.is_empty() {
        return None;
    }
    let mut ranges: Vec<String> = Vec::new();
    let mut total = 0usize;
    for script in &scripts {
        for cmd in script.split(';').map(str::trim).filter(|c| !c.is_empty()) {
            let addr = cmd.strip_suffix('p')?.trim();
            let (a, b) = match addr.split_once(',') {
                Some((a, b)) => (a.trim(), b.trim()),
                None => (addr, addr),
            };
            let a: usize = a.parse().ok().filter(|a| *a > 0)?;
            if b == "$" {
                ranges.push(format!("{a}-"));
                total += 2000;
            } else {
                let b: usize = b.parse().ok().filter(|b| *b >= a)?;
                ranges.push(if a == b { a.to_string() } else { format!("{a}-{b}") });
                total += b + 1 - a;
            }
        }
    }
    // Never cut what sed would have printed whole.
    let max = total.clamp(1, 5000);
    Some(format!("{head} read --max-lines {max} {} {}", files[0], ranges.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(cmd: &str) -> Option<String> {
        rewrite(cmd, "qx", None).map(|r| r.command)
    }

    #[test]
    fn greps_of_files_become_searches() {
        assert_eq!(
            rw(r#"grep -n "soundmap\|sound_bake" server.py | head -40"#).as_deref(),
            Some(r#"qx nav --hook search --basic --head 40 -- "soundmap\|sound_bake" server.py"#)
        );
        assert_eq!(
            rw("grep -rn -B2 'rfd::' src/ 2>/dev/null").as_deref(),
            Some("qx nav --hook search --basic -B 2 -- 'rfd::' src/ 2>/dev/null")
        );
        assert_eq!(
            rw("grep -rnE 'a|b' --include=*.rs src | wc -l").as_deref(),
            Some("qx nav --hook search -c --glob '*.rs' -- 'a|b' src")
        );
        assert_eq!(rw("grep -c x a.rs b.rs").as_deref(), Some("qx nav --hook search --basic -c -- x a.rs b.rs"));
    }

    #[test]
    fn line_reads_become_reads() {
        assert_eq!(rw("sed -n 360,520p src/main.rs").as_deref(), Some("qx nav --hook read --max-lines 161 src/main.rs 360-520"));
        assert_eq!(
            rw("sed -n '1,10p;20,30p' src/a.rs").as_deref(),
            Some("qx nav --hook read --max-lines 21 src/a.rs 1-10,20-30")
        );
        assert_eq!(rw("sed -n '5p' a.rs").as_deref(), Some("qx nav --hook read --max-lines 1 a.rs 5"));
    }

    #[test]
    fn sequences_keep_their_other_parts() {
        assert_eq!(
            rw("cd /w/space_soup && sed -n 1,9p src/a.rs; echo ----; sed -n 20,29p src/b.rs").as_deref(),
            Some("cd /w/space_soup && qx nav --hook read --max-lines 9 src/a.rs 1-9; echo ----; qx nav --hook read --max-lines 10 src/b.rs 20-29")
        );
        assert_eq!(
            rw("S=/tmp/x; grep -n WHERE $S/log.txt | head -5").as_deref(),
            Some("S=/tmp/x; qx nav --hook search --basic --head 5 -- WHERE $S/log.txt")
        );
    }

    #[test]
    fn anything_not_fully_understood_runs_as_written() {
        for cmd in [
            "cargo test 2>&1 | grep -E 'error|warning'", // filters output
            "grep -v foo a.rs",                         // inverted
            "grep -o 'x[0-9]' a.rs",                    // only-matching
            "grep foo a.rs | tail -5",                  // last matches
            "grep foo a.rs | sort | uniq -c",           // further processing
            "grep foo a.rs && echo found",              // grep's exit status matters
            "grep -rn foo --exclude-dir=docs .",        // an exclusion we do not apply
            "grep -n \"$(cat pat)\" a.rs",              // command substitution
            "grep foo a.rs > out.txt",                  // redirection
            "sed -n '/fn main/,/^}/p' src/main.rs",     // regex addresses
            "sed -n 1,10p a.rs b.rs",                   // line numbers run across files
            "sed 's/a/b/' a.rs",                        // an edit, not a read
            "grep -P '(?<=x)y' a.rs",                   // perl syntax
            "grep foo",                                 // stdin
            "QX_RAW=1 grep -n foo a.rs",                // asked for the real thing
            "grep -n 'a\\1' a.rs",                      // a backreference
        ] {
            assert_eq!(rw(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn the_manifest_and_binary_are_passed_quoted() {
        let r = rewrite("grep -n x a.rs", "/A B/qx", Some("/w s/.cortex/index-sources.json")).unwrap();
        assert_eq!(r.command, "/A B/qx nav --sources-from '/w s/.cortex/index-sources.json' --hook search --basic -- x a.rs");
        assert_eq!(r.kinds, vec!["search"]);
    }
}
