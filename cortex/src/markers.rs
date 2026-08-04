/// Phase 0C: CORTEX-* knowledge marker parser.
///
/// Parses structured XML-like tags that agents embed in their responses:
///
///   [CORTEX-PATTERN: name="..." intent="..." trust="verified" uses="..."]
///   body text
///   [/CORTEX-PATTERN]
///
///   [CORTEX-AP: description="..." tags="..."]
///   wrong: ...
///   correct: ...
///   [/CORTEX-AP]
///
///   [CORTEX-CORRECTION: attempted="..." reason="..." fix="..."][/CORTEX-CORRECTION]
///
///   [CORTEX-ADR: title="..." tags="..."]
///   Context: ...
///   [/CORTEX-ADR]
///
///   [CORTEX-PREFS-NOTE: tags="..."]
///   note text
///   [/CORTEX-PREFS-NOTE]
///
///   [CORTEX-SKILL-CANDIDATE: name="..." trigger="..."]
///   summary text
///   [/CORTEX-SKILL-CANDIDATE]
use std::collections::HashMap;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum KnowledgeMarker {
    Pattern {
        name:    String,
        intent:  String,
        body:    String,
        trust:   String,
        uses:    Vec<String>,
        tags:    Vec<String>,
    },
    AntiPattern {
        description: String,
        wrong:       String,
        correct:     String,
        tags:        Vec<String>,
    },
    Correction {
        attempted: String,
        reason:    String,
        fix:       String,
        tags:      Vec<String>,
    },
    Adr {
        title:   String,
        context: String,
        decision: String,
        tags:    Vec<String>,
    },
    PrefsNote {
        body: String,
        tags: Vec<String>,
    },
    SkillCandidate {
        name:    String,
        trigger: String,
        summary: String,
    },
}

impl KnowledgeMarker {
    /// Short marker-type label for DB storage.
    pub fn marker_type(&self) -> &'static str {
        match self {
            KnowledgeMarker::Pattern { .. }        => "pattern",
            KnowledgeMarker::AntiPattern { .. }    => "anti_pattern",
            KnowledgeMarker::Correction { .. }     => "correction",
            KnowledgeMarker::Adr { .. }            => "adr",
            KnowledgeMarker::PrefsNote { .. }      => "prefs_note",
            KnowledgeMarker::SkillCandidate { .. } => "skill_candidate",
        }
    }

    /// Primary name for the marker (used in summaries).
    pub fn display_name(&self) -> String {
        match self {
            KnowledgeMarker::Pattern { name, .. }        => name.clone(),
            KnowledgeMarker::AntiPattern { description, .. } => description.chars().take(60).collect(),
            KnowledgeMarker::Correction { attempted, .. } => attempted.chars().take(60).collect(),
            KnowledgeMarker::Adr { title, .. }           => title.clone(),
            KnowledgeMarker::PrefsNote { body, .. }      => body.chars().take(60).collect(),
            KnowledgeMarker::SkillCandidate { name, .. } => name.clone(),
        }
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Parse all CORTEX-* markers from arbitrary text (typically an assistant response).
pub fn parse_markers(text: &str) -> Vec<KnowledgeMarker> {
    let mut results = Vec::new();

    let marker_types = [
        "CORTEX-PATTERN",
        "CORTEX-AP",
        "CORTEX-CORRECTION",
        "CORTEX-ADR",
        "CORTEX-PREFS-NOTE",
        "CORTEX-SKILL-CANDIDATE",
    ];

    for mtype in &marker_types {
        let open_prefix = format!("[{}:", mtype);
        let close_tag   = format!("[/{}]", mtype);

        let mut search_from = 0usize;
        while let Some(open_pos) = find_case_insensitive(text, &open_prefix, search_from) {
            // Find the closing `]` of the opening tag, ignoring any that sit
            // inside a quoted attribute value.
            let Some(header_end) = find_header_end(&text[open_pos..]) else { break; };
            let header_end_abs = open_pos + header_end;

            // Extract the attribute string: text between ":" and "]".
            let attr_start = open_pos + open_prefix.len();
            let attrs_str  = &text[attr_start..header_end_abs];
            let attrs      = parse_attrs(attrs_str);

            // Find the closing tag.
            let body_start = header_end_abs + 1;
            let close_pos  = find_case_insensitive(text, &close_tag, body_start);
            let (body, next_search) = if let Some(cp) = close_pos {
                let body = text[body_start..cp].trim().to_string();
                (body, cp + close_tag.len())
            } else {
                // No closing tag — take the rest of the line as body.
                let end = text[body_start..].find('\n')
                    .map(|n| body_start + n)
                    .unwrap_or(text.len());
                (text[body_start..end].trim().to_string(), end)
            };

            if let Some(marker) = build_marker(mtype, &attrs, &body) {
                results.push(marker);
            }

            search_from = next_search;
        }
    }

    results
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Byte offset of the `]` that closes a marker's opening tag.
///
/// A naive `find(']')` truncates any attribute whose value legitimately contains
/// a bracket — and since attributes after the truncation point are lost too, the
/// entry lands with a half-written description and **no tags at all**, silently.
/// That really happened: an anti-pattern documenting the regex `[a-z_]+` was
/// stored as "Rust field-name regexes using [a-z_" with its tags dropped.
///
/// So track quote state and only accept a bracket outside a quoted value. If the
/// quotes turn out to be unbalanced, fall back to the first bracket rather than
/// swallowing the rest of the document.
fn find_header_end(s: &str) -> Option<usize> {
    let mut in_quotes = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ']' if !in_quotes => return Some(i),
            // A marker header is a single line; a newline means the quotes never
            // closed, so stop guessing and use the naive result.
            '\n' if in_quotes => return s.find(']'),
            _ => {}
        }
    }
    s.find(']')
}

fn find_case_insensitive(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let lower_h = haystack.to_lowercase();
    let lower_n = needle.to_lowercase();
    lower_h[from..].find(&lower_n).map(|p| from + p)
}

/// Parse `key="value" key2="value2"` attribute string into a HashMap.
/// Handles both `key="value"` and `key=value` (no quotes) forms.
fn parse_attrs(attrs: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut remaining = attrs.trim();

    while !remaining.is_empty() {
        // Skip whitespace
        remaining = remaining.trim_start();
        if remaining.is_empty() { break; }

        // Find the '='
        let Some(eq_pos) = remaining.find('=') else { break; };
        let key = remaining[..eq_pos].trim().to_lowercase();
        remaining = &remaining[eq_pos + 1..];

        let value = if remaining.starts_with('"') {
            // Quoted value — find closing quote (not escaped).
            let inner = &remaining[1..];
            let end = inner.find('"').unwrap_or(inner.len());
            let v = inner[..end].to_string();
            remaining = if end + 1 < inner.len() { &inner[end + 1..] } else { "" };
            v
        } else {
            // Unquoted — read until whitespace or end.
            let end = remaining.find([' ', '\t', '\n', '\r'])
                .unwrap_or(remaining.len());
            let v = remaining[..end].trim().to_string();
            remaining = &remaining[end..];
            v
        };

        if !key.is_empty() {
            map.insert(key, value);
        }
    }

    map
}

/// Build a typed KnowledgeMarker from parsed attributes and body.
fn build_marker(mtype: &str, attrs: &HashMap<String, String>, body: &str) -> Option<KnowledgeMarker> {
    let get = |k: &str| attrs.get(k).cloned().unwrap_or_default();

    match mtype.to_uppercase().as_str() {
        "CORTEX-PATTERN" => {
            let name = get("name");
            let intent = get("intent");
            if name.is_empty() && intent.is_empty() && body.is_empty() { return None; }
            let uses_str = get("uses");
            let uses = if uses_str.is_empty() {
                vec![]
            } else {
                uses_str.split(',').map(|s| s.trim().to_string()).collect()
            };
            let tags_str = get("tags");
            let tags = if tags_str.is_empty() {
                vec![]
            } else {
                tags_str.split(',').map(|s| s.trim().to_string()).collect()
            };
            Some(KnowledgeMarker::Pattern {
                name: if name.is_empty() { "unnamed-pattern".to_string() } else { name },
                intent: if intent.is_empty() { body.chars().take(80).collect() } else { intent },
                body: body.to_string(),
                trust: if get("trust").is_empty() { "annotated".to_string() } else { get("trust") },
                uses,
                tags,
            })
        }
        "CORTEX-AP" => {
            // Body may contain "wrong: ...\ncorrect: ..." or standalone text.
            let description = get("description");
            if description.is_empty() && body.is_empty() { return None; }
            let (wrong, correct) = parse_wrong_correct(body);
            let tags_str = get("tags");
            let tags = if tags_str.is_empty() {
                vec![]
            } else {
                tags_str.split(',').map(|s| s.trim().to_string()).collect()
            };
            Some(KnowledgeMarker::AntiPattern {
                description: if description.is_empty() { body.chars().take(200).collect() } else { description },
                wrong,
                correct,
                tags,
            })
        }
        "CORTEX-CORRECTION" => {
            let attempted = get("attempted");
            let reason    = get("reason");
            let fix       = get("fix");
            if attempted.is_empty() && body.is_empty() { return None; }
            let tags_str = get("tags");
            let tags = if tags_str.is_empty() {
                vec![]
            } else {
                tags_str.split(',').map(|s| s.trim().to_string()).collect()
            };
            Some(KnowledgeMarker::Correction {
                attempted: if attempted.is_empty() { body.chars().take(200).collect() } else { attempted },
                reason,
                fix,
                tags,
            })
        }
        "CORTEX-ADR" => {
            let title = get("title");
            if title.is_empty() && body.is_empty() { return None; }
            let tags_str = get("tags");
            let tags = if tags_str.is_empty() {
                vec![]
            } else {
                tags_str.split(',').map(|s| s.trim().to_string()).collect()
            };
            // Try to split body into context/decision lines.
            let (ctx, decision) = parse_adr_body(body);
            Some(KnowledgeMarker::Adr {
                title: if title.is_empty() { body.chars().take(80).collect() } else { title },
                context: ctx,
                decision,
                tags,
            })
        }
        "CORTEX-PREFS-NOTE" => {
            if body.is_empty() { return None; }
            let tags_str = get("tags");
            let tags = if tags_str.is_empty() {
                vec![]
            } else {
                tags_str.split(',').map(|s| s.trim().to_string()).collect()
            };
            Some(KnowledgeMarker::PrefsNote {
                body: body.to_string(),
                tags,
            })
        }
        "CORTEX-SKILL-CANDIDATE" => {
            let name    = get("name");
            let trigger = get("trigger");
            if name.is_empty() && body.is_empty() { return None; }
            Some(KnowledgeMarker::SkillCandidate {
                name: if name.is_empty() { "unnamed-skill".to_string() } else { name },
                trigger,
                summary: body.to_string(),
            })
        }
        _ => None,
    }
}

/// Parse "wrong: ...\ncorrect: ..." lines from CORTEX-AP body.
fn parse_wrong_correct(body: &str) -> (String, String) {
    let mut wrong   = String::new();
    let mut correct = String::new();
    let mut in_wrong   = false;
    let mut in_correct = false;

    for line in body.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("wrong:") {
            wrong = line[6..].trim().to_string();
            in_wrong = true; in_correct = false;
        } else if lower.starts_with("correct:") {
            correct = line[8..].trim().to_string();
            in_correct = true; in_wrong = false;
        } else if in_wrong && !line.trim().is_empty() {
            wrong.push('\n');
            wrong.push_str(line);
        } else if in_correct && !line.trim().is_empty() {
            correct.push('\n');
            correct.push_str(line);
        }
    }

    if wrong.is_empty() { wrong = body.chars().take(200).collect(); }
    if correct.is_empty() { correct = "see body above".to_string(); }
    (wrong, correct)
}

/// Parse ADR body looking for "Context:" and "Decision:" prefixes.
fn parse_adr_body(body: &str) -> (String, String) {
    let mut ctx      = String::new();
    let mut decision = String::new();
    let mut mode     = "context";

    for line in body.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("context:") || lower.starts_with("context: ") {
            ctx = line[8..].trim().to_string();
            mode = "context";
        } else if lower.starts_with("decision:") || lower.starts_with("decision: ") {
            decision = line[9..].trim().to_string();
            mode = "decision";
        } else if mode == "context" && !line.trim().is_empty() {
            if !ctx.is_empty() { ctx.push(' '); }
            ctx.push_str(line.trim());
        } else if mode == "decision" && !line.trim().is_empty() {
            if !decision.is_empty() { decision.push(' '); }
            decision.push_str(line.trim());
        }
    }

    if ctx.is_empty() { ctx = body.chars().take(400).collect(); }
    if decision.is_empty() { decision = "see context".to_string(); }
    (ctx, decision)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pattern_marker() {
        let text = r#"Some text before.

[CORTEX-PATTERN: name="gif-compose" intent="Compose GIF delta frames" trust="verified" uses="AnimatedSprite"]
Blit each frame's pixels onto a running canvas_buf.
[/CORTEX-PATTERN]

More text."#;
        let markers = parse_markers(text);
        assert_eq!(markers.len(), 1);
        match &markers[0] {
            KnowledgeMarker::Pattern { name, intent, trust, .. } => {
                assert_eq!(name, "gif-compose");
                assert_eq!(intent, "Compose GIF delta frames");
                assert_eq!(trust, "verified");
            }
            _ => panic!("expected Pattern"),
        }
    }

    #[test]
    fn parse_anti_pattern_marker() {
        let text = r#"[CORTEX-AP: description="No .frame() method on AnimatedSprite" tags="AnimatedSprite,frame"]
wrong: animated_sprite.frame(3)
correct: decode GIF manually into Vec<Arc<RgbaImage>>
[/CORTEX-AP]"#;
        let markers = parse_markers(text);
        assert_eq!(markers.len(), 1);
        match &markers[0] {
            KnowledgeMarker::AntiPattern { description, wrong, correct, tags } => {
                assert!(description.contains("frame"));
                assert!(wrong.contains("frame(3)"));
                assert!(correct.contains("decode"));
                assert!(tags.contains(&"AnimatedSprite".to_string()));
            }
            _ => panic!("expected AntiPattern"),
        }
    }

    #[test]
    fn parse_correction_marker() {
        let text = r#"[CORTEX-CORRECTION: attempted="Used raw frame.buffer" reason="delta-encoded" fix="blit onto canvas_buf"][/CORTEX-CORRECTION]"#;
        let markers = parse_markers(text);
        assert_eq!(markers.len(), 1);
        match &markers[0] {
            KnowledgeMarker::Correction { attempted, reason, fix, .. } => {
                assert!(attempted.contains("frame.buffer"));
                assert!(reason.contains("delta"));
                assert!(fix.contains("canvas_buf"));
            }
            _ => panic!("expected Correction"),
        }
    }

    #[test]
    fn parse_multiple_markers_in_one_text() {
        let text = "[CORTEX-AP: description=\"test\"][/CORTEX-AP] [CORTEX-PREFS-NOTE: tags=\"a\"]a note[/CORTEX-PREFS-NOTE]";
        let markers = parse_markers(text);
        assert_eq!(markers.len(), 2);
    }

    #[test]
    fn empty_text_yields_no_markers() {
        assert!(parse_markers("no markers here").is_empty());
    }

    #[test]
    fn attrs_parse_correctly() {
        let attrs = parse_attrs(r#"name="hello world" trust="verified" tags="a,b,c""#);
        assert_eq!(attrs["name"], "hello world");
        assert_eq!(attrs["trust"], "verified");
        assert_eq!(attrs["tags"], "a,b,c");
    }
}

#[cfg(test)]
mod bracket_in_attribute_tests {
    use super::*;

    /// Regression: the real marker that was corrupted in production.
    #[test]
    fn a_bracket_inside_a_quoted_value_does_not_truncate_the_header() {
        let text = r#"[CORTEX-AP: description="Rust field-name regexes using [a-z_]+ silently skip fields containing digits" tags="regex,rust,parsing"]wrong: grep -E "pub [a-z_]+:"
correct: use [a-z_0-9]+[/CORTEX-AP]"#;
        let markers = parse_markers(text);
        assert_eq!(markers.len(), 1, "marker should parse");
        match &markers[0] {
            KnowledgeMarker::AntiPattern { description, tags, .. } => {
                assert!(
                    description.contains("silently skip fields containing digits"),
                    "description truncated at the bracket: {description}"
                );
                assert_eq!(tags, &vec!["regex".to_string(), "rust".to_string(), "parsing".to_string()],
                           "tags after a bracketed value must survive");
            }
            other => panic!("expected an anti-pattern, got {other:?}"),
        }
    }

    #[test]
    fn brackets_in_several_attributes_and_in_the_body_all_survive() {
        let text = r#"[CORTEX-PATTERN: name="glob-[abc]" intent="match [a-z] ranges" trust="verified" uses="regex"]body with [brackets] here[/CORTEX-PATTERN]"#;
        let markers = parse_markers(text);
        assert_eq!(markers.len(), 1);
        match &markers[0] {
            KnowledgeMarker::Pattern { name, intent, body, .. } => {
                assert_eq!(name, "glob-[abc]");
                assert_eq!(intent, "match [a-z] ranges");
                assert_eq!(body, "body with [brackets] here");
            }
            other => panic!("expected a pattern, got {other:?}"),
        }
    }

    /// Ordinary markers must be unaffected.
    #[test]
    fn markers_without_brackets_parse_exactly_as_before() {
        let text = r#"[CORTEX-AP: description="plain description" tags="a,b"]wrong: x
correct: y[/CORTEX-AP]"#;
        let markers = parse_markers(text);
        assert_eq!(markers.len(), 1);
        match &markers[0] {
            KnowledgeMarker::AntiPattern { description, wrong, correct, tags } => {
                assert_eq!(description, "plain description");
                assert!(wrong.contains('x'));
                assert!(correct.contains('y'));
                assert_eq!(tags.len(), 2);
            }
            other => panic!("expected an anti-pattern, got {other:?}"),
        }
    }

    /// An unbalanced quote must not swallow the rest of the transcript.
    #[test]
    fn an_unclosed_quote_falls_back_instead_of_consuming_everything() {
        let text = "[CORTEX-AP: description=\"oops unclosed tags=\"x\"]wrong: a\ncorrect: b[/CORTEX-AP]\nlater text";
        let markers = parse_markers(text);
        assert!(markers.len() <= 1, "must not run away past the marker");
    }
}
