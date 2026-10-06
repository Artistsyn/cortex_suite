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

// ── PowerShell ──────────────────────────────────────────────────────────────
//
// The same reads written the PowerShell way, for agents whose shell is
// PowerShell (Claude Code's `PowerShell` tool, which Windows gets when there
// is no Git Bash; VS Code's terminal on Windows): `Select-String` where bash
// greps, `Get-Content | Select-Object` where it reads with `sed -n`. Only what
// is fully understood is taken, as above, and the answer is a PowerShell
// command too - `& 'quartz-ctx' nav ...` - that parses in Windows PowerShell
// 5.1 as well as 7: no `&&` the command did not already use, no `--`, every
// value single-quoted and none holding a double quote, which 5.1 passes to a
// program unescaped.

/// A PowerShell word in argument mode, or an operator.
#[derive(Debug, Clone, PartialEq)]
enum PsTok {
    /// `val` once quotes are removed. `bare` when no part of it was quoted:
    /// only a bare `-Name` is a parameter, and only a bare `a,b` an array.
    Word { raw: String, val: String, bare: bool },
    Op(&'static str),
}

/// The quote characters PowerShell accepts besides `'` and `"`.
fn ps_typographic_quote(c: char) -> bool {
    matches!(c, '\u{2018}'..='\u{201E}')
}

/// Words and operators of one PowerShell command line. `None` for anything
/// this does not model: variables, subexpressions, script blocks, arrays
/// written with `@`, the call operator, redirection other than `2>$null`,
/// escapes, comments, more than one line.
fn ps_tokenize(cmd: &str) -> Option<Vec<PsTok>> {
    let b: Vec<char> = cmd.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            ' ' | '\t' => {
                i += 1;
                continue;
            }
            '|' if b.get(i + 1) == Some(&'|') => {
                toks.push(PsTok::Op("||"));
                i += 2;
                continue;
            }
            '|' => {
                toks.push(PsTok::Op("|"));
                i += 1;
                continue;
            }
            '&' if b.get(i + 1) == Some(&'&') => {
                toks.push(PsTok::Op("&&"));
                i += 2;
                continue;
            }
            ';' => {
                toks.push(PsTok::Op(";"));
                i += 1;
                continue;
            }
            '2' if b.get(i + 1) == Some(&'>') => {
                let word: String = b[i..].iter().take_while(|c| !matches!(c, ' ' | '\t' | ';' | '|')).collect();
                if word.eq_ignore_ascii_case("2>$null") {
                    toks.push(PsTok::Op("2>$null"));
                    i += word.chars().count();
                    continue;
                }
                return None;
            }
            // A dash PowerShell also reads as one (en, em, horizontal bar)
            // would make the word a parameter.
            '\n' | '\r' | '&' | '<' | '>' | '(' | ')' | '{' | '}' | '@' | '$' | '#' | '`' | ','
            | '\u{2013}'..='\u{2015}' => return None,
            c if ps_typographic_quote(c) => return None,
            _ => {}
        }
        let (mut raw, mut val, mut bare) = (String::new(), String::new(), true);
        while i < b.len() {
            match b[i] {
                ' ' | '\t' | '|' | ';' => break,
                '\n' | '\r' | '&' | '<' | '>' | '(' | ')' | '{' | '}' | '$' | '`' => return None,
                '\'' => {
                    // Literal text; '' is one quote.
                    let mut j = i + 1;
                    raw.push('\'');
                    loop {
                        match *b.get(j)? {
                            '\'' if b.get(j + 1) == Some(&'\'') => {
                                raw.push_str("''");
                                val.push('\'');
                                j += 2;
                            }
                            '\'' => break,
                            c if ps_typographic_quote(c) => return None,
                            c => {
                                raw.push(c);
                                val.push(c);
                                j += 1;
                            }
                        }
                    }
                    raw.push('\'');
                    i = j + 1;
                    bare = false;
                }
                '"' => {
                    // Expandable: taken only where nothing in it would expand.
                    let mut j = i + 1;
                    raw.push('"');
                    loop {
                        match *b.get(j)? {
                            '"' if b.get(j + 1) == Some(&'"') => {
                                raw.push_str("\"\"");
                                val.push('"');
                                j += 2;
                            }
                            '"' => break,
                            '`' => return None,
                            '$' if b.get(j + 1).is_some_and(|n| n.is_alphanumeric() || "_?^$:{(".contains(*n)) => {
                                return None
                            }
                            c if ps_typographic_quote(c) => return None,
                            c => {
                                raw.push(c);
                                val.push(c);
                                j += 1;
                            }
                        }
                    }
                    raw.push('"');
                    i = j + 1;
                    bare = false;
                }
                c if ps_typographic_quote(c) => return None,
                c => {
                    raw.push(c);
                    val.push(c);
                    i += 1;
                }
            }
        }
        toks.push(PsTok::Word { raw, val, bare });
    }
    Some(toks)
}

/// A cmdlet's parameters: name, aliases, and whether it takes a value. Taken
/// from PowerShell 7.6 (`(Get-Command X).Parameters`), which 5.1's are a
/// subset of: a short name that is ambiguous here may be unique in 5.1, where
/// the command then works and the rewrite is only skipped.
type PsParams = &'static [(&'static str, &'static [&'static str], bool)];

/// The common parameters. They take part in resolving a shortened name: `-e`
/// on Select-String is ambiguous partly because of `-ErrorAction`.
const PS_COMMON: PsParams = &[
    ("Verbose", &["vb"], false),
    ("Debug", &["db"], false),
    ("ErrorAction", &["ea"], true),
    ("WarningAction", &["wa"], true),
    ("InformationAction", &["infa"], true),
    ("ProgressAction", &["proga"], true),
    ("ErrorVariable", &["ev"], true),
    ("WarningVariable", &["wv"], true),
    ("InformationVariable", &["iv"], true),
    ("OutVariable", &["ov"], true),
    ("OutBuffer", &["ob"], true),
    ("PipelineVariable", &["pv"], true),
];

const PS_SELECT_STRING: PsParams = &[
    ("Culture", &[], true),
    ("InputObject", &[], true),
    ("Pattern", &[], true),
    ("Path", &[], true),
    ("LiteralPath", &["PSPath", "LP"], true),
    ("Raw", &[], false),
    ("SimpleMatch", &[], false),
    ("CaseSensitive", &[], false),
    ("Quiet", &[], false),
    ("List", &[], false),
    ("NoEmphasis", &[], false),
    ("Include", &[], true),
    ("Exclude", &[], true),
    ("NotMatch", &[], false),
    ("AllMatches", &[], false),
    ("Encoding", &[], true),
    ("Context", &[], true),
];

const PS_GET_CHILD_ITEM: PsParams = &[
    ("Path", &[], true),
    ("LiteralPath", &["PSPath", "LP"], true),
    ("Filter", &[], true),
    ("Include", &[], true),
    ("Exclude", &[], true),
    ("Recurse", &["s", "r"], false),
    ("Depth", &[], true),
    ("Force", &[], false),
    ("Name", &[], false),
    ("Attributes", &[], true),
    ("FollowSymlink", &[], false),
    ("Directory", &["ad"], false),
    ("File", &["af"], false),
    ("Hidden", &["ah", "h"], false),
    ("ReadOnly", &["ar"], false),
    ("System", &["as"], false),
];

const PS_GET_CONTENT: PsParams = &[
    ("ReadCount", &[], true),
    ("TotalCount", &["First", "Head"], true),
    ("Tail", &["Last"], true),
    ("Path", &[], true),
    ("LiteralPath", &["PSPath", "LP"], true),
    ("Filter", &[], true),
    ("Include", &[], true),
    ("Exclude", &[], true),
    ("Force", &[], false),
    ("Credential", &[], true),
    ("Delimiter", &[], true),
    ("Wait", &[], false),
    ("Raw", &[], false),
    ("Encoding", &[], true),
    ("AsByteStream", &[], false),
    ("Stream", &[], true),
];

const PS_SELECT_OBJECT: PsParams = &[
    ("InputObject", &[], true),
    ("Property", &[], true),
    ("ExcludeProperty", &[], true),
    ("ExpandProperty", &[], true),
    ("Unique", &[], false),
    ("CaseInsensitive", &[], false),
    ("Last", &[], true),
    ("First", &[], true),
    ("Skip", &[], true),
    ("SkipLast", &[], true),
    ("Wait", &[], false),
    ("Index", &[], true),
    ("SkipIndex", &[], true),
];

const PS_MEASURE_OBJECT: PsParams = &[
    ("InputObject", &[], true),
    ("Property", &[], true),
    ("StandardDeviation", &[], false),
    ("Sum", &[], false),
    ("AllStats", &[], false),
    ("Average", &[], false),
    ("Maximum", &[], false),
    ("Minimum", &[], false),
    ("Line", &[], false),
    ("Word", &[], false),
    ("Character", &[], false),
    ("IgnoreWhiteSpace", &[], false),
];

/// A command PowerShell runs as one of the cmdlets modelled here: its name,
/// parameters, and positional parameters in order. `ls` and `cat` are those
/// cmdlets on Windows only; elsewhere they run the system's own programs.
fn ps_cmdlet(name: &str, windows: bool) -> Option<(&'static str, PsParams, &'static [&'static str])> {
    Some(match name.to_ascii_lowercase().as_str() {
        "select-string" | "sls" => ("Select-String", PS_SELECT_STRING, &["Pattern", "Path"]),
        "get-childitem" | "gci" | "dir" => ("Get-ChildItem", PS_GET_CHILD_ITEM, &["Path", "Filter"]),
        "ls" if windows => ("Get-ChildItem", PS_GET_CHILD_ITEM, &["Path", "Filter"]),
        "get-content" | "gc" | "type" => ("Get-Content", PS_GET_CONTENT, &["Path"]),
        "cat" if windows => ("Get-Content", PS_GET_CONTENT, &["Path"]),
        "select-object" | "select" => ("Select-Object", PS_SELECT_OBJECT, &["Property"]),
        "measure-object" | "measure" => ("Measure-Object", PS_MEASURE_OBJECT, &["Property"]),
        _ => return None,
    })
}

/// Commands kept as written around a rewritten read.
fn ps_kept(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "cd" | "set-location" | "sl" | "chdir" | "pushd" | "push-location" | "echo" | "write-output" | "write" | "write-host"
    )
}

/// The parameter `-name` binds to: an exact name or alias, else the one
/// parameter whose name or an alias starts with it (PowerShell's rule:
/// `-Patt` is Pattern, `-Pat` is ambiguous with Path, `-h` is Head).
fn ps_param(name: &str, params: PsParams) -> Option<(&'static str, bool)> {
    let all = || params.iter().chain(PS_COMMON.iter());
    let exact = |s: &str| s.eq_ignore_ascii_case(name);
    if let Some(p) = all().find(|p| exact(p.0) || p.1.iter().any(|a| exact(a))) {
        return Some((p.0, p.2));
    }
    let n = name.to_ascii_lowercase();
    let starts = |s: &str| s.to_ascii_lowercase().starts_with(&n);
    let hits: Vec<_> = all().filter(|p| starts(p.0) || p.1.iter().any(|a| starts(a))).collect();
    match hits.as_slice() {
        [p] => Some((p.0, p.2)),
        _ => None,
    }
}

/// An argument as written: its value, and whether it was bare.
#[derive(Debug, Clone)]
struct PsArg {
    val: String,
    bare: bool,
}

impl PsArg {
    /// The values of an array parameter: a bare `a,b` is two.
    fn items(&self) -> Option<Vec<String>> {
        if self.bare && self.val.contains(',') {
            let v: Vec<String> = self.val.split(',').map(str::to_string).collect();
            (!v.iter().any(String::is_empty)).then_some(v)
        } else {
            Some(vec![self.val.clone()])
        }
    }

    /// A single value: an array here would mean something else.
    fn one(&self) -> Option<&str> {
        (!(self.bare && self.val.contains(','))).then_some(self.val.as_str())
    }

    fn count(&self) -> Option<usize> {
        self.one()?.parse().ok()
    }
}

/// One cmdlet call, bound the way PowerShell binds it.
struct PsCall {
    cmd: &'static str,
    named: Vec<(&'static str, Option<PsArg>)>,
    /// Errors silenced: `2>$null`, `-ErrorAction SilentlyContinue`.
    silent: bool,
}

impl PsCall {
    fn has(&self, name: &str) -> bool {
        self.named.iter().any(|(n, _)| *n == name)
    }

    fn value(&self, name: &str) -> Option<&PsArg> {
        self.named.iter().find(|(n, _)| *n == name).and_then(|(_, v)| v.as_ref())
    }

    /// Whether every parameter given is one of `known`.
    fn only(&self, known: &[&str]) -> bool {
        self.named.iter().all(|(n, _)| known.contains(n))
    }

    /// `value(name)` as a count, `Some(None)` when not given.
    fn count(&self, name: &str) -> Option<Option<usize>> {
        match self.value(name) {
            Some(a) => Some(Some(a.count()?)),
            None => Some(None),
        }
    }
}

/// One element of a pipeline as a cmdlet call; `None` for any other command,
/// an unknown or ambiguous parameter, or a parameter given twice.
fn ps_call(elem: &[PsTok], windows: bool) -> Option<PsCall> {
    let mut silent = false;
    let mut words: Vec<(&str, bool)> = Vec::new();
    for t in elem {
        match t {
            PsTok::Op("2>$null") => silent = true,
            PsTok::Op(_) => return None,
            // `-Name:'value'` and the like: not modelled.
            PsTok::Word { raw, bare: false, .. } if raw.starts_with('-') => return None,
            PsTok::Word { val, bare, .. } => words.push((val, *bare)),
        }
    }
    let ((name, true), args) = words.split_first()? else { return None };
    let (cmd, params, positions) = ps_cmdlet(name, windows)?;
    let mut named: Vec<(&'static str, Option<PsArg>)> = Vec::new();
    let mut positional: Vec<PsArg> = Vec::new();
    let (mut i, mut only_args) = (0, false);
    while i < args.len() {
        let (val, bare) = args[i];
        i += 1;
        if !only_args && bare && val == "--" {
            only_args = true;
            continue;
        }
        let is_param = !only_args && bare && val.starts_with('-') && val[1..].starts_with(|c: char| c.is_alphabetic() || c == '_');
        if !is_param {
            positional.push(PsArg { val: val.to_string(), bare });
            continue;
        }
        let (pname, attached) = match val[1..].split_once(':') {
            Some((n, v)) => (n, Some(v)),
            None => (&val[1..], None),
        };
        let (canon, takes) = ps_param(pname, params)?;
        let value = match (takes, attached) {
            (true, Some(v)) => Some(PsArg { val: v.to_string(), bare: true }),
            (true, None) => {
                let (v, b) = *args.get(i)?;
                i += 1;
                Some(PsArg { val: v.to_string(), bare: b })
            }
            (false, None) => None,
            (false, Some(_)) => return None, // `-Recurse:$false` and the like
        };
        if named.iter().any(|(n, _)| *n == canon) {
            return None;
        }
        named.push((canon, value));
    }
    // Positionals fill the positional parameters not already named, in order.
    let free: Vec<&'static str> = positions.iter().copied().filter(|p| !named.iter().any(|(n, _)| n == p)).collect();
    if positional.len() > free.len() {
        return None;
    }
    named.extend(free.into_iter().zip(positional.into_iter().map(Some)));
    if let Some(at) = named.iter().position(|(n, _)| *n == "ErrorAction") {
        match named[at].1.as_ref()?.one()?.to_ascii_lowercase().as_str() {
            "silentlycontinue" | "ignore" | "0" | "4" => silent = true,
            "continue" | "2" => {}
            _ => return None,
        }
        named.remove(at);
    }
    Some(PsCall { cmd, named, silent })
}

/// Single-quoted for PowerShell, with every character it reads as a single
/// quote doubled.
pub fn ps_quote(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}'..='\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// A value for nav's command line, quoted. `None` for what would not arrive
/// intact or would be read as something else: empty (5.1 drops it), a double
/// quote (5.1 passes it unescaped), a leading dash (a flag), a line break, or
/// a trailing backslash after a space (it would escape the quote 5.1 adds).
fn ps_value(s: &str) -> Option<String> {
    let broken = s.is_empty()
        || s.starts_with('-')
        || s.contains(['"', '\n', '\r', '\0'])
        || (s.contains(char::is_whitespace) && s.ends_with('\\'));
    (!broken).then(|| ps_quote(s))
}

/// A path as PowerShell's `-Path` takes it, with a wildcard only in its last
/// part, which is all nav expands. `[` is a wildcard there too.
fn ps_path_ok(p: &str) -> bool {
    if p.starts_with('~') || p.contains(['[', ']']) {
        return false;
    }
    let last = p.rfind(['/', '\\']).map_or(0, |i| i + 1);
    !p[..last].contains(['*', '?'])
}

/// A .NET pattern, as Select-String reads it, in the form nav reads the same
/// way (`grep -E`, with `\|` taken as `|`). `None` where Rust's regex would
/// read the text differently and nothing short of a translation of the class
/// syntax would fix it: nested classes and set operations (`[[:alpha:]]`,
/// `&&`, `--`, `~~`), `{,n}`, an escaped backslash before `|`. What Rust
/// cannot read at all (lookaround, backreferences) fails nav's own check.
fn dotnet_pattern(p: &str) -> Option<String> {
    let c: Vec<char> = p.chars().collect();
    let mut out = String::new();
    let (mut i, mut class) = (0, false);
    while i < c.len() {
        let ch = c[i];
        if ch == '\\' {
            let n = *c.get(i + 1)?;
            match (class, n) {
                // A literal `|`, which nav would take for alternation.
                (false, '|') => out.push_str("[|]"),
                (true, '|') => out.push('|'),
                // Literal in .NET; word boundaries in Rust.
                (false, '<' | '>') => out.push(n),
                _ => {
                    out.push('\\');
                    out.push(n);
                }
            }
            i += 2;
            continue;
        }
        if class {
            match ch {
                '[' => return None,
                '&' | '-' | '~' if c.get(i + 1) == Some(&ch) => return None,
                ']' => class = false,
                _ => {}
            }
            out.push(ch);
            i += 1;
            continue;
        }
        match ch {
            '[' => {
                class = true;
                out.push('[');
                i += 1;
                // A leading `^`, then a leading `]`, belong to the class.
                if c.get(i) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
                if c.get(i) == Some(&']') {
                    out.push(']');
                    i += 1;
                }
                continue;
            }
            '{' if c.get(i + 1) == Some(&',') => return None,
            _ => out.push(ch),
        }
        i += 1;
    }
    (!class && !out.contains("\\|")).then_some(out)
}

/// What a Select-String call searches for, and where.
struct PsSearch {
    pattern: String,
    fixed: bool,
    icase: bool,
    before: usize,
    after: usize,
    globs: Vec<String>,
    paths: Vec<String>,
}

/// Select-String's own arguments, given the files piped into it, if any:
/// (paths, name globs).
fn ps_select_string(s: &PsCall, input: Option<(Vec<String>, Vec<String>)>) -> Option<PsSearch> {
    let known = ["Pattern", "Path", "LiteralPath", "SimpleMatch", "CaseSensitive", "AllMatches", "NoEmphasis", "Raw", "Context", "Include"];
    if !s.only(&known) {
        return None;
    }
    let pattern = s.value("Pattern")?.one()?.to_string();
    let (before, after) = match s.value("Context") {
        None => (0, 0),
        Some(a) => match a.items()?.as_slice() {
            [n] => {
                let n = n.parse().ok()?;
                (n, n)
            }
            [b, a] => (b.parse().ok()?, a.parse().ok()?),
            _ => return None,
        },
    };
    let include = match s.value("Include") {
        Some(a) => a.items()?,
        None => Vec::new(),
    };
    let (paths, globs) = match input {
        // Piped files: a -Path as well would be read too.
        Some(piped) => {
            if s.has("Path") || s.has("LiteralPath") || !include.is_empty() {
                return None;
            }
            piped
        }
        None => {
            let mut paths = Vec::new();
            if let Some(a) = s.value("Path") {
                for p in a.items()? {
                    ps_path_ok(&p).then_some(())?;
                    paths.push(p);
                }
            }
            if let Some(a) = s.value("LiteralPath") {
                for p in a.items()? {
                    (!p.starts_with('~') && !p.contains(['*', '?'])).then_some(())?;
                    paths.push(p);
                }
            }
            // No path: it reads the pipeline, which is not modelled.
            if paths.is_empty() {
                return None;
            }
            (paths, include)
        }
    };
    if globs.iter().any(|g| g.contains(['[', ']', '{', '}', '/', '\\'])) {
        return None;
    }
    Some(PsSearch { pattern, fixed: s.has("SimpleMatch"), icase: !s.has("CaseSensitive"), before, after, globs, paths })
}

/// The files `Get-ChildItem` lists into Select-String: (paths, name globs).
fn ps_child_items(g: &PsCall) -> Option<(Vec<String>, Vec<String>)> {
    if !g.only(&["Path", "LiteralPath", "Filter", "Include", "Recurse", "File"]) {
        return None;
    }
    let mut paths = Vec::new();
    for key in ["Path", "LiteralPath"] {
        if let Some(a) = g.value(key) {
            for p in a.items()? {
                if p.starts_with('~') || p.contains(['*', '?', '[', ']']) {
                    return None;
                }
                paths.push(p);
            }
        }
    }
    let filter = match g.value("Filter") {
        Some(a) => Some(a.one()?.to_string()),
        None => None,
    };
    let include = match g.value("Include") {
        Some(a) => a.items()?,
        None => Vec::new(),
    };
    // Both apply, and nav's globs are alternatives.
    if filter.is_some() && !include.is_empty() {
        return None;
    }
    let globs: Vec<String> = filter.into_iter().chain(include.iter().cloned()).collect();
    if paths.is_empty() {
        paths.push(".".to_string());
    }
    if g.has("Recurse") {
        return Some((paths, globs));
    }
    // One directory's own files, where the path is surely a directory; and
    // -Include lists nothing without -Recurse.
    if !include.is_empty() {
        return None;
    }
    let mut own = Vec::new();
    for p in paths {
        let dir = p.trim_end_matches(['/', '\\']);
        if dir.is_empty() || !(dir == "." || dir.len() < p.len()) {
            return None;
        }
        own.push(format!("{dir}/*"));
    }
    Some((own, globs))
}

/// The one file `Get-Content` reads.
fn ps_content_file(c: &PsCall) -> Option<String> {
    let p = match (c.value("Path"), c.value("LiteralPath")) {
        (Some(a), None) => a.one()?.to_string(),
        (None, Some(a)) => a.one()?.to_string(),
        _ => return None,
    };
    // A wildcard would read several files as one stream.
    (!p.starts_with('~') && !p.contains(['*', '?', '[', ']'])).then_some(p)
}

/// The lines `Get-Content` [`| Select-Object`] prints, as nav's line range
/// and how many lines that is. `None` for the whole file, which is a `cat`,
/// and for what cannot be said without knowing the file's length.
fn ps_read_range(c: &PsCall, sel: Option<&PsCall>) -> Option<(String, usize)> {
    if !c.only(&["Path", "LiteralPath", "TotalCount", "Tail"]) {
        return None;
    }
    let (total, tail) = (c.count("TotalCount")?, c.count("Tail")?);
    let (skip, first, last) = match sel {
        None => (None, None, None),
        Some(s) => {
            if !s.only(&["First", "Skip", "Last"]) {
                return None;
            }
            (s.count("Skip")?, s.count("First")?, s.count("Last")?)
        }
    };
    if [total, tail, first, last].contains(&Some(0)) {
        return None;
    }
    let skip = skip.unwrap_or(0);
    let range = |a: usize, b: usize| if a == b { a.to_string() } else { format!("{a}-{b}") };
    match (total, tail, first, last) {
        (None, Some(t), None, None) if skip == 0 => Some((format!("-{t}"), t)),
        (Some(t), None, None, None) => (skip < t).then(|| (range(skip + 1, t), t - skip)),
        (Some(t), None, Some(f), None) => (skip < t).then(|| {
            let b = t.min(skip + f);
            (range(skip + 1, b), b - skip)
        }),
        (Some(t), None, None, Some(l)) if skip == 0 => {
            let a = t.saturating_sub(l) + 1;
            Some((range(a, t), t + 1 - a))
        }
        (None, None, Some(f), None) => Some((range(skip + 1, skip + f), f)),
        (None, None, None, Some(l)) if skip == 0 => Some((format!("-{l}"), l)),
        // To the end, as sed's `A,$p` is taken.
        (None, None, None, None) if skip > 0 => Some((format!("{}-", skip + 1), 2000)),
        _ => None,
    }
}

/// `(Get-Content FILE)[A..B]`, zero-based, and `[N]`, `[-N..-1]`: the form
/// that needs parentheses, taken only as a whole command. (file, range, lines,
/// silent)
fn ps_index_read(cmd: &str, windows: bool) -> Option<(String, String, usize, bool)> {
    let rest = cmd.strip_prefix('(')?;
    let close = rest.find(')')?;
    let index = rest[close + 1..].trim().strip_prefix('[')?.strip_suffix(']')?;
    let c = ps_call(&ps_tokenize(&rest[..close])?, windows)?;
    if c.cmd != "Get-Content" || !c.only(&["Path", "LiteralPath"]) {
        return None;
    }
    let file = ps_content_file(&c)?;
    let num = |s: &str| s.trim().parse::<i64>().ok();
    let (a, b) = match index.split_once("..") {
        Some((a, b)) => (num(a)?, num(b)?),
        None => {
            let n = num(index)?;
            (n, n)
        }
    };
    let (range, lines) = if a >= 0 && b >= a {
        let (a, b) = (a as usize + 1, b as usize + 1);
        (if a == b { a.to_string() } else { format!("{a}-{b}") }, b + 1 - a)
    } else if a < 0 && b == -1 {
        (format!("-{}", -a), (-a) as usize)
    } else {
        return None;
    };
    Some((file, range, lines, c.silent))
}

/// nav's `search` arguments for a Select-String.
fn ps_search_args(s: &PsSearch, head: Option<usize>, count: bool) -> Option<String> {
    let pattern = if s.fixed { s.pattern.clone() } else { dotnet_pattern(&s.pattern)? };
    let probe = crate::nav::SearchOpts { fixed: s.fixed, ignore_case: s.icase, ..Default::default() };
    crate::nav::check_pattern(&pattern, &probe).ok()?;
    let mut a = String::from("search");
    if s.icase {
        a.push_str(" -i");
    }
    if s.fixed {
        a.push_str(" -F");
    }
    if s.before == s.after && s.before > 0 {
        a.push_str(&format!(" -C {}", s.before));
    } else {
        if s.before > 0 {
            a.push_str(&format!(" -B {}", s.before));
        }
        if s.after > 0 {
            a.push_str(&format!(" -A {}", s.after));
        }
    }
    if count {
        a.push_str(" -c");
    }
    if !s.globs.is_empty() {
        a.push_str(&format!(" --glob {}", ps_value(&s.globs.join(","))?));
    }
    if let Some(n) = head {
        a.push_str(&format!(" --head {n}"));
    }
    a.push_str(&format!(" {}", ps_value(&pattern)?));
    for p in &s.paths {
        a.push_str(&format!(" {}", ps_value(p)?));
    }
    Some(a)
}

/// One pipeline, rewritten: the command and the kind of read it was.
fn ps_pipeline(elems: &[&[PsTok]], head: &str, windows: bool) -> Option<(String, &'static str)> {
    let calls: Vec<PsCall> = elems.iter().map(|e| ps_call(e, windows)).collect::<Option<_>>()?;
    let names: Vec<&str> = calls.iter().map(|c| c.cmd).collect();
    let (body, kind) = match names.as_slice() {
        ["Get-Content"] | ["Get-Content", "Select-Object"] => {
            let file = ps_content_file(&calls[0])?;
            let (range, lines) = ps_read_range(&calls[0], calls.get(1))?;
            (format!("read --max-lines {} {} {range}", lines.clamp(1, 5000), ps_value(&file)?), "read")
        }
        _ => {
            let at = names.iter().position(|n| *n == "Select-String")?;
            let input = match &names[..at] {
                [] => None,
                ["Get-ChildItem"] => Some(ps_child_items(&calls[0])?),
                ["Get-Content"] => {
                    calls[0].only(&["Path", "LiteralPath"]).then_some(())?;
                    Some((vec![ps_content_file(&calls[0])?], Vec::new()))
                }
                _ => return None,
            };
            let s = ps_select_string(&calls[at], input)?;
            // `| Select-Object -First N` counts matches, and with context a
            // match is several lines: only without it is that `--head N`.
            let (head_n, count) = match &calls[at + 1..] {
                [] => (None, false),
                [sel] if sel.cmd == "Select-Object" && sel.only(&["First"]) && s.before + s.after == 0 => {
                    (Some(sel.value("First")?.count().filter(|n| *n > 0)?), false)
                }
                [m] if m.cmd == "Measure-Object" && m.named.is_empty() => (None, true),
                _ => return None,
            };
            (ps_search_args(&s, head_n, count)?, "search")
        }
    };
    let silent = calls.iter().any(|c| c.silent);
    Some((format!("{head} {body}{}", if silent { " 2>$null" } else { "" }), kind))
}

/// The navigation command answering a PowerShell `cmd`, or `None` to run it
/// as written. `exe` is quartz-ctx's path, unquoted; `manifest` as for
/// [`rewrite`].
pub fn rewrite_powershell(cmd: &str, exe: &str, manifest: Option<&str>) -> Option<Rewrite> {
    rewrite_ps(cmd, exe, manifest, cfg!(windows))
}

fn rewrite_ps(cmd: &str, exe: &str, manifest: Option<&str>, windows: bool) -> Option<Rewrite> {
    // `$env:QX_RAW=1`, `$env:QX_RAW = '1'`: asked for the real thing.
    if cmd.contains("QX_RAW") {
        return None;
    }
    let head = match manifest {
        Some(m) => format!("& {} nav --sources-from {} --hook", ps_quote(exe), ps_value(m)?),
        None => format!("& {} nav --hook", ps_quote(exe)),
    };
    let cmd = cmd.trim();
    if let Some((file, range, lines, silent)) = ps_index_read(cmd, windows) {
        let quiet = if silent { " 2>$null" } else { "" };
        let command = format!("{head} read --max-lines {} {} {range}{quiet}", lines.clamp(1, 5000), ps_value(&file)?);
        return Some(Rewrite { command, kinds: vec!["read"] });
    }
    // Split into statements at `;` and `&&`, keeping the separators.
    let mut segments: Vec<(Vec<PsTok>, Option<&'static str>)> = Vec::new();
    let mut cur: Vec<PsTok> = Vec::new();
    for t in ps_tokenize(cmd)? {
        match t {
            PsTok::Op(sep @ (";" | "&&")) => segments.push((std::mem::take(&mut cur), Some(sep))),
            PsTok::Op("||") => return None,
            t => cur.push(t),
        }
    }
    segments.push((cur, None));

    let mut out = String::new();
    let mut kinds = Vec::new();
    for (seg, sep) in &segments {
        if seg.is_empty() {
            if sep.is_some() {
                return None;
            }
            continue;
        }
        let elems: Vec<&[PsTok]> = seg.split(|t| *t == PsTok::Op("|")).collect();
        if elems.iter().any(|e| e.is_empty()) {
            return None;
        }
        let PsTok::Word { val: first, bare: true, .. } = &elems[0][0] else { return None };
        if elems.len() == 1 && ps_kept(first) {
            let raw: Vec<&str> = seg
                .iter()
                .map(|t| match t {
                    PsTok::Word { raw, .. } => raw.as_str(),
                    PsTok::Op(o) => o,
                })
                .collect();
            out.push_str(&raw.join(" "));
        } else {
            // A read's success is not the original's: an `&&` after one
            // would change what runs next.
            if *sep == Some("&&") {
                return None;
            }
            let (text, kind) = ps_pipeline(&elems, &head, windows)?;
            out.push_str(&text);
            kinds.push(kind);
        }
        if let Some(sep) = sep {
            out.push_str(if *sep == ";" { "; " } else { " && " });
        }
    }
    (!kinds.is_empty()).then(|| Rewrite { command: out.trim().to_string(), kinds })
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

    fn ps(cmd: &str) -> Option<String> {
        rewrite_ps(cmd, "qx", None, false).map(|r| r.command)
    }

    #[test]
    fn powershell_searches_become_searches() {
        assert_eq!(
            ps("Select-String -Path src/*.rs -Pattern 'fn main'").as_deref(),
            Some("& 'qx' nav --hook search -i 'fn main' 'src/*.rs'")
        );
        // Case-sensitive only when asked; the pattern is positional 0.
        assert_eq!(ps(r#"sls "TODO" src/main.rs -CaseSensitive"#).as_deref(), Some("& 'qx' nav --hook search 'TODO' 'src/main.rs'"));
        assert_eq!(
            ps("Get-ChildItem -Recurse -Filter *.rs | Select-String -Pattern 'impl Foo' | Select-Object -First 20").as_deref(),
            Some("& 'qx' nav --hook search -i --glob '*.rs' --head 20 'impl Foo' '.'")
        );
        assert_eq!(
            ps("gci src -r -Include *.rs,*.toml | sls 'x' -Context 2,3").as_deref(),
            Some("& 'qx' nav --hook search -i -B 2 -A 3 --glob '*.rs,*.toml' 'x' 'src'")
        );
        // A literal pipe stays one; with -SimpleMatch the text is literal anyway.
        assert_eq!(ps(r"Select-String -Path a.rs -Pattern 'a\|b'").as_deref(), Some("& 'qx' nav --hook search -i 'a[|]b' 'a.rs'"));
        assert_eq!(
            ps(r"Select-String -Path a.rs -Pattern 'a\|b' -SimpleMatch").as_deref(),
            Some(r"& 'qx' nav --hook search -i -F 'a\|b' 'a.rs'")
        );
        assert_eq!(ps("Get-Content a.rs | Select-String 'x' | Measure-Object").as_deref(), Some("& 'qx' nav --hook search -i -c 'x' 'a.rs'"));
        // One directory's own files, and errors silenced the cmdlet's way.
        assert_eq!(
            ps("Get-ChildItem src/ -Filter *.rs | Select-String x -ErrorAction SilentlyContinue").as_deref(),
            Some("& 'qx' nav --hook search -i --glob '*.rs' 'x' 'src/*' 2>$null")
        );
    }

    #[test]
    fn powershell_line_reads_become_reads() {
        assert_eq!(
            ps("Get-Content src/main.rs | Select-Object -Skip 99 -First 50").as_deref(),
            Some("& 'qx' nav --hook read --max-lines 50 'src/main.rs' 100-149")
        );
        assert_eq!(ps("Get-Content src/main.rs -TotalCount 40").as_deref(), Some("& 'qx' nav --hook read --max-lines 40 'src/main.rs' 1-40"));
        assert_eq!(ps("gc -Path src/main.rs -Tail 30").as_deref(), Some("& 'qx' nav --hook read --max-lines 30 'src/main.rs' -30"));
        assert_eq!(ps("(Get-Content src/main.rs)[99..149]").as_deref(), Some("& 'qx' nav --hook read --max-lines 51 'src/main.rs' 100-150"));
        assert_eq!(ps("(gc a.rs)[-5..-1]").as_deref(), Some("& 'qx' nav --hook read --max-lines 5 'a.rs' -5"));
        // `-h` is short for -Head, an alias of -TotalCount.
        assert_eq!(ps("Get-Content a.rs -h 100 | select -Last 10").as_deref(), Some("& 'qx' nav --hook read --max-lines 10 'a.rs' 91-100"));
        assert_eq!(ps("type a.rs | select -Skip 10").as_deref(), Some("& 'qx' nav --hook read --max-lines 2000 'a.rs' 11-"));
    }

    #[test]
    fn powershell_statements_keep_their_other_parts() {
        assert_eq!(
            ps("cd src; Select-String -Path *.rs -Pattern foo; echo ----").as_deref(),
            Some("cd src; & 'qx' nav --hook search -i 'foo' '*.rs'; echo ----")
        );
        assert_eq!(
            ps("Set-Location C:/w && gc a.rs -First 5").as_deref(),
            Some("Set-Location C:/w && & 'qx' nav --hook read --max-lines 5 'a.rs' 1-5")
        );
    }

    #[test]
    fn powershell_anything_not_fully_understood_runs_as_written() {
        for cmd in [
            "Select-String -Pattern x",                                               // reads the pipeline
            "Select-String -Path a.rs -Pattern x -NotMatch",                          // inverted
            "Select-String -Pat x -Path a.rs",                                        // ambiguous: Pattern or Path
            "Select-String -Path a.rs -Pattern x -C 2",                               // ambiguous: Culture, CaseSensitive, Context
            "$env:QX_RAW=1; Select-String -Path a.rs -Pattern x",                     // asked for the real thing
            "Get-Content a.rs",                                                       // the whole file: a cat
            "Get-Content a.rs | Select-Object -Skip 2 -Last 5",                       // needs the file's length
            "Select-String -Path a.rs -Pattern x | ForEach-Object { $_.Line }",       // further processing
            "Select-String -Path a.rs -Pattern x -Context 2 | Select-Object -First 3", // a match with context is several lines
            "Select-String -Path a.rs -Pattern x && echo found",                      // the read's success matters
            "Select-String -Path a.rs -Pattern '(?<=a)b'",                            // lookbehind: not in nav's regex
            "Select-String -Path a.rs -Pattern '[[:alpha:]]'",                        // a class the two engines read differently
            "Select-String -Path a.rs -Pattern 'say \"hi\"'",                         // 5.1 would pass the quote unescaped
            "Get-ChildItem -Recurse | Select-String x -Path y.rs",                    // two sources of files
            "Get-ChildItem -Recurse -Depth 2 | Select-String x",                      // a depth nav does not apply
            "Select-String -Path a.rs -Pattern x > out.txt",                          // redirection
            "ls -r | sls x",                                                          // `ls` is the system's own program here
            "Select-String -Path src/*/a.rs -Pattern x",                              // a wildcard before the last part
            "Select-String -Path a.rs -Pattern \"$name\"",                            // a variable
            "Get-ChildItem src | Select-String x",                                    // src may be a file
            "Select-String -Path a.rs -Pattern x -Encoding utf8",                     // a decoding nav does not do
            "Get-Content a.rs -Raw | Select-Object -First 1",                         // one string, not lines
            "(Get-Content a.rs)[5..2]",                                               // backwards
            "Select-String -Path a.rs -Pattern a,b",                                  // several patterns
        ] {
            assert_eq!(ps(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn powershell_aliases_follow_the_platform() {
        let win = |c: &str| rewrite_ps(c, "qx", None, true).map(|r| r.command);
        assert_eq!(win("ls -r -Filter *.rs | sls x").as_deref(), Some("& 'qx' nav --hook search -i --glob '*.rs' 'x' '.'"));
        assert_eq!(win("cat a.rs -TotalCount 3").as_deref(), Some("& 'qx' nav --hook read --max-lines 3 'a.rs' 1-3"));
        assert_eq!(ps("cat a.rs -TotalCount 3"), None);
    }

    #[test]
    fn powershell_values_are_single_quoted_for_51() {
        let r = rewrite_ps(
            r#"Select-String -Path 'C:\My Dir\a.rs' -Pattern "it's""#,
            r"C:\Program Files\qx.exe",
            Some(r"C:\w s\.cortex\index-sources.json"),
            true,
        )
        .unwrap();
        assert_eq!(
            r.command,
            r"& 'C:\Program Files\qx.exe' nav --sources-from 'C:\w s\.cortex\index-sources.json' --hook search -i 'it''s' 'C:\My Dir\a.rs'"
        );
        assert_eq!(r.kinds, vec!["search"]);
    }

    #[test]
    fn dotnet_patterns_read_the_same_in_nav() {
        assert_eq!(dotnet_pattern(r"a\|b").as_deref(), Some("a[|]b"));
        assert_eq!(dotnet_pattern(r"Vec\<u8\>").as_deref(), Some("Vec<u8>"));
        assert_eq!(dotnet_pattern(r"[\|x]").as_deref(), Some("[|x]"));
        assert_eq!(dotnet_pattern(r"[]a]b").as_deref(), Some("[]a]b"));
        assert_eq!(dotnet_pattern(r"fn \w+\(").as_deref(), Some(r"fn \w+\("));
        for p in [r"[a-z-[aeiou]]", r"[a&&b]", r"x{,3}", r"\\|x", r"[abc"] {
            assert_eq!(dotnet_pattern(p), None, "{p}");
        }
    }
}
