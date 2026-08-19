//! Redaction for the MCP call log.
//!
//! `mcp_calls.args` stores the arguments of every tool call. That is harmless
//! for authored text and dangerous for captured output: `compact_output`
//! receives the complete stdout+stderr of every command an agent runs, so the
//! call log silently becomes a transcript of everything that ever crossed a
//! terminal — credentials echoed by a script, tokens in a URL, a colleague's
//! name and email out of `git log`.
//!
//! That is not hypothetical. On the reference store, `compact_output` held
//! 4,773 rows and 6.84 MB of the 7.57 MB total, and two of those rows carried a
//! third party's email address captured from `git log` output. It was found
//! only because the database was about to be published.
//!
//! Two properties make it worse than an ordinary logging mistake:
//!
//! 1. **A table query does not reveal it.** The payload is one JSON string in
//!    one column, so `SELECT ... WHERE args LIKE '%secret%'` on the columns you
//!    think matter comes back clean while the bytes sit in the file.
//! 2. **`VACUUM` does not remove it.** These are live rows, not freed pages.
//!
//! So the store cannot be made safe after the fact by tidying. It has to not be
//! captured in the first place, which is what this module does at the single
//! chokepoint every call passes through (`Store::log_mcp_call`).

/// Tools whose arguments are captured command output rather than authored text.
///
/// Nothing reads these. The only consumer of `args` is the closeout fallback
/// that scrapes `[CORTEX-*]` markers, and markers come from the agent's own
/// prose — never from the stdout of a command. Storing the payload buys
/// nothing and risks everything.
const OUTPUT_BEARING: &[&str] = &["compact_output"];

/// Cap for anything still stored. The marker scrape reads recent calls looking
/// for `[CORTEX-...]` tags; those are far below this. A single argument blob
/// larger than this is bulk data, not authored intent.
const MAX_ARGS_BYTES: usize = 8192;

/// Bytes held back from the cap for the truncation marker itself, so a
/// truncated value lands inside the limit and is not re-truncated next pass.
const TRUNCATION_RESERVE: usize = 48;

/// Commands are one line of intent, not bulk data. Anything past this is a
/// heredoc or an inlined script, which is not what forensics needs.
const MAX_COMMAND_BYTES: usize = 2048;

/// Token prefixes that identify a credential regardless of surrounding syntax.
/// Deliberately conservative: every entry here is a published, unambiguous
/// prefix, so a false positive cannot silently mangle authored knowledge.
const SECRET_PREFIXES: &[&str] = &[
    "ghp_", "gho_", "ghu_", "ghs_", "ghr_", // GitHub
    "github_pat_",
    "sk-",        // OpenAI and lookalikes
    "sk_live_", "sk_test_", "rk_live_", // Stripe
    "xoxb-", "xoxp-", "xoxa-", "xoxs-", // Slack
    "AKIA", "ASIA",                     // AWS access key ids
    "AIza",                             // Google
    "hf_",                              // Hugging Face
    "glpat-",                           // GitLab
];

/// Multi-line secrets, redacted from the marker to the end of the value.
const BLOCK_MARKERS: &[&str] = &["-----BEGIN"];

/// Leading marker of an already-redacted payload, used to make redaction
/// idempotent. Must match what `redact_call_args` emits.
const REDACTED_PREFIX: &str = r#"{"_redacted":"#;

/// Apply the storage policy for one call's arguments.
///
/// Returns what should actually be written to `mcp_calls.args`.
pub fn redact_call_args(tool: &str, args: &str) -> String {
    // Already redacted: leave it exactly as-is.
    //
    // Without this the stub is re-redacted on every pass, and because it embeds
    // `bytes` = the length of what it replaced, each run reports the length of
    // the PREVIOUS stub instead of the original payload. The retroactive scrub
    // then rewrites every row forever and the size telemetry decays to noise.
    if args.starts_with(REDACTED_PREFIX) {
        return args.to_string();
    }

    if OUTPUT_BEARING.contains(&tool) {
        // Keep the shape and the size — the telemetry that reads this table
        // counts calls and reports volume — and drop the payload entirely.
        return format!(
            r#"{{"_redacted":"captured command output","tool":"{tool}","bytes":{}}}"#,
            args.len()
        );
    }

    let mut out = scrub_secrets(args);
    if out.len() > MAX_ARGS_BYTES {
        // Cut to leave room for the marker, so the RESULT is within the cap.
        //
        // Truncating to exactly MAX_ARGS_BYTES and then appending the marker
        // leaves the row over the limit, so the next pass truncates it again --
        // the retroactive scrub then rewrites those rows on every single run,
        // for ever, each time reporting a slightly different byte count.
        let budget = MAX_ARGS_BYTES.saturating_sub(TRUNCATION_RESERVE);
        // Byte-index cuts panic mid-codepoint; one unicode constant once killed
        // a whole extraction run that way.
        let mut cut = budget.min(out.len());
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        let dropped = out.len() - cut;
        out.truncate(cut);
        out.push_str(&format!("...[{dropped} bytes truncated]"));
        debug_assert!(out.len() <= MAX_ARGS_BYTES, "truncation overshot its own cap");
    }
    out
}

/// Replace credential-shaped tokens with a marker, leaving everything else
/// byte-identical.
pub fn scrub_secrets(s: &str) -> String {
    if !looks_risky(s) {
        return s.to_string();
    }

    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;

    'outer: while i < bytes.len() {
        for m in BLOCK_MARKERS {
            if s[i..].starts_with(m) {
                out.push_str("[REDACTED key block]");
                // Skip to the end of the PEM block, or the end of input.
                match s[i..].find("-----END") {
                    Some(rel) => {
                        let after = i + rel;
                        let end = s[after..].find('\n').map(|n| after + n).unwrap_or(bytes.len());
                        i = end;
                    }
                    None => i = bytes.len(),
                }
                continue 'outer;
            }
        }
        for p in SECRET_PREFIXES {
            if s[i..].starts_with(p) && at_token_start(s, i) {
                let end = token_end(s, i);
                // A bare prefix with nothing after it is not a credential.
                if end - i > p.len() + 4 {
                    out.push_str("[REDACTED credential]");
                    i = end;
                    continue 'outer;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Cheap pre-filter: most arguments contain none of these, and scanning every
/// byte of every call for a dozen prefixes is wasted work on a hot path.
fn looks_risky(s: &str) -> bool {
    SECRET_PREFIXES.iter().any(|p| s.contains(p))
        || BLOCK_MARKERS.iter().any(|m| s.contains(m))
}

/// Is byte index `i` the start of a token? Prevents `task-sk-1` matching `sk-`.
fn at_token_start(s: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    match s[..i].chars().next_back() {
        Some(c) => !(c.is_alphanumeric() || c == '_' || c == '-'),
        None => true,
    }
}

/// End of the credential-shaped run starting at `i`.
fn token_end(s: &str, i: usize) -> usize {
    let mut end = i;
    for c in s[i..].chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bug this module exists for: compact_output's args are the full
    // stdout/stderr of arbitrary commands, so anything a command printed was
    // being retained verbatim and indefinitely.
    #[test]
    fn captured_command_output_is_not_stored() {
        let leak = r#"{"command":"git log","stdout":"Author: Someone <someone@example.com>\nAWS_SECRET=hunter2"}"#;
        let got = redact_call_args("compact_output", leak);
        assert!(!got.contains("example.com"), "email survived: {got}");
        assert!(!got.contains("hunter2"), "secret survived: {got}");
        assert!(got.contains("\"bytes\":"), "size telemetry must survive: {got}");
    }

    // The one consumer of args scrapes CORTEX-* markers, so authored tool
    // arguments must pass through untouched or closeout silently loses them.
    // The retroactive scrub runs over rows it may have already rewritten. If a
    // stub is re-redacted, `bytes` reports the previous stub's length rather
    // than the original payload's, and every row is rewritten on every pass.
    #[test]
    fn redaction_is_idempotent() {
        let original = r#"{"command":"ls","stdout":"a lot of output here"}"#;
        let once = redact_call_args("compact_output", original);
        let twice = redact_call_args("compact_output", &once);
        assert_eq!(once, twice, "second pass changed the stub");
        assert!(once.contains(&format!("\"bytes\":{}", original.len())));
    }

    #[test]
    fn authored_arguments_pass_through_unchanged() {
        let markers = "[CORTEX-AP: description=\"a thing\"]wrong: x\ncorrect: y[/CORTEX-AP]";
        assert_eq!(redact_call_args("closeout_session", markers), markers);
    }

    #[test]
    fn credentials_are_scrubbed_from_authored_arguments() {
        for secret in [
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ012345",
            "AKIAIOSFODNN7EXAMPLE",
            "xoxb-123456789012-abcdefghijkl",
        ] {
            let got = scrub_secrets(&format!("token is {secret} ok"));
            assert!(!got.contains(secret), "{secret} survived: {got}");
            assert!(got.contains("[REDACTED credential]"));
            assert!(got.ends_with(" ok"), "surrounding text must survive: {got}");
        }
    }

    #[test]
    fn private_key_blocks_are_scrubbed() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpQIBAAK\n-----END RSA PRIVATE KEY-----\nafter";
        let got = scrub_secrets(pem);
        assert!(!got.contains("MIIEpQIBAAK"), "key body survived: {got}");
        assert!(got.contains("before") && got.contains("after"));
    }

    // A conservative scrubber is the point: mangling real knowledge to chase a
    // maybe-secret would make the store lie, which is worse than a log entry.
    #[test]
    fn ordinary_text_is_untouched() {
        for benign in [
            "the sk- prefix identifies an OpenAI key",
            "task-sk-1 is a ticket id",
            "AKIA is the AWS access key prefix",
            "no secrets here at all",
        ] {
            assert_eq!(scrub_secrets(benign), benign, "mangled: {benign}");
        }
    }

    #[test]
    fn oversized_arguments_are_capped_on_a_char_boundary() {
        // Multi-byte char straddling the cap: a byte-index truncate panics
        // mid-codepoint, which is how one unicode constant once killed an
        // entire extraction run.
        let big = format!("{}{}", "x".repeat(MAX_ARGS_BYTES - 1), "é".repeat(4096));
        let got = redact_call_args("get_context", &big);

        assert!(got.len() < big.len(), "not capped: {} vs {}", got.len(), big.len());
        assert!(got.contains("bytes truncated"));
        assert!(
            got.len() <= MAX_ARGS_BYTES,
            "result must land inside the cap or it re-truncates for ever, got {}",
            got.len()
        );
        assert_eq!(
            redact_call_args("get_context", &got),
            got,
            "truncation must be idempotent"
        );
        // Valid UTF-8 by construction if we cut on a boundary; String cannot
        // hold anything else, so assert the boundary logic actually ran.
        assert!(!got.starts_with('\u{fffd}'));
    }
}

/// Redact a command line before it is stored for forensics.
///
/// A second capture surface, separate from tool arguments: the command itself
/// can carry the credential (`-H "Authorization: Bearer ..."`, a password in a
/// database URL, `export API_KEY=`). `command_family` is what analysis reads,
/// so the full text is forensic only and loses nothing by being scrubbed.
pub fn redact_command(command: &str) -> String {
    let mut out = scrub_secrets(command);
    if out.len() > MAX_COMMAND_BYTES {
        let budget = MAX_COMMAND_BYTES.saturating_sub(TRUNCATION_RESERVE);
        let mut cut = budget.min(out.len());
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        let dropped = out.len() - cut;
        out.truncate(cut);
        out.push_str(&format!("...[{dropped} bytes truncated]"));
    }
    out
}

// ── Retroactive scrub ────────────────────────────────────────────────────────

/// Apply the current redaction policy to rows already in the call log.
///
/// The write-time fix only protects calls made from now on. A store that has
/// been running for months already holds the payloads, and neither a table
/// query nor `VACUUM` will surface or clear them — they are live rows.
///
/// Non-destructive: rewrites `args` in place and deletes nothing, so call
/// history, counts and session grouping are all preserved. Idempotent — a
/// second run finds nothing left to change.
///
/// Returns (rows rewritten, bytes removed).
pub fn scrub_existing_log(conn: &rusqlite::Connection) -> rusqlite::Result<(usize, u64)> {
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, tool, args FROM mcp_calls")?;
        let mapped = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut changed = 0usize;
    let mut freed = 0u64;
    for (id, tool, args) in rows {
        let safe = redact_call_args(&tool, &args);
        if safe == args {
            continue;
        }
        freed += args.len().saturating_sub(safe.len()) as u64;
        conn.execute("UPDATE mcp_calls SET args = ?1 WHERE id = ?2", rusqlite::params![safe, id])?;
        changed += 1;
    }
    // Second surface: the stored command line.
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, command FROM compression_savings")?;
        let mapped = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, command) in rows {
        let safe = redact_command(&command);
        if safe == command {
            continue;
        }
        freed += command.len().saturating_sub(safe.len()) as u64;
        conn.execute(
            "UPDATE compression_savings SET command = ?1 WHERE id = ?2",
            rusqlite::params![safe, id],
        )?;
        changed += 1;
    }

    // Third surface: the test-outcome ledger's command column.
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, command FROM test_outcomes")?;
        let mapped = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, command) in rows {
        let safe = redact_command(&command);
        if safe == command {
            continue;
        }
        freed += command.len().saturating_sub(safe.len()) as u64;
        conn.execute(
            "UPDATE test_outcomes SET command = ?1 WHERE id = ?2",
            rusqlite::params![safe, id],
        )?;
        changed += 1;
    }

    Ok((changed, freed))
}

#[cfg(test)]
mod command_tests {
    use super::*;

    // The second capture surface. Found only after fixing the first: the args
    // scrub cleared mcp_calls and 3 email occurrences survived, in the command
    // column of compression_savings.
    #[test]
    fn credentials_in_a_command_line_are_scrubbed() {
        let cmd = r#"curl -H "Authorization: Bearer ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123" https://api"#;
        let got = redact_command(cmd);
        assert!(!got.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"), "token survived: {got}");
        assert!(got.contains("curl") && got.contains("https://api"), "shape lost: {got}");
    }

    #[test]
    fn ordinary_commands_are_untouched() {
        for cmd in ["cargo build --release", "git log --oneline -5", "ls -la ~/src"] {
            assert_eq!(redact_command(cmd), cmd);
        }
    }

    #[test]
    fn command_redaction_is_idempotent() {
        let cmd = format!("echo {}", "x".repeat(MAX_COMMAND_BYTES * 2));
        let once = redact_command(&cmd);
        assert_eq!(redact_command(&once), once, "second pass changed it");
        assert!(once.len() <= MAX_COMMAND_BYTES);
    }
}
