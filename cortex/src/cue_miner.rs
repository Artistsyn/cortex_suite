//! Proposing and judging cue changes from labelled prompts
//! (docs/self-learning-loop-2026-09-30.md, L4).
//!
//! The challenge hook's cues were tuned by hand: a person noticed a miss, and
//! an agent replayed candidate phrases over every real prompt and read what
//! each would change. This does the mechanical part. From a labelled miss it
//! proposes candidates -- capitalised emphasis words, and word n-grams tried in
//! each cue list -- and judges each against the labelled corpus with the gate
//! the hand change used:
//!
//! - it must fix the miss it came from;
//! - it must break no labelled prompt (a new false fire, or a right answer
//!   turned wrong);
//! - every unlabelled prompt it changes must be labelled first.
//!
//! What it cannot do from replay is show that a change generalises FORWARD:
//! at about five disputes a week, a phrase from one miss rarely recurs soon.
//! That evidence comes from shadow mode, before anything is promoted.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fire {
    None,
    Generic,
    Limit,
}

/// The detector's lists, owned so candidates can be tried.
#[derive(Clone, Debug, Default)]
pub struct CueSet {
    pub emphasis: Vec<String>,
    pub limit: Vec<String>,
    pub generic: Vec<String>,
    pub words: Vec<String>,
    pub phrases: Vec<String>,
}

impl CueSet {
    /// What `corrections::detect` ships.
    pub fn shipped() -> Self {
        use crate::corrections::{CUES, LIMIT_CUES, LIMIT_EMPHASIS, LIMIT_PHRASES, LIMIT_WORDS};
        Self {
            emphasis: LIMIT_EMPHASIS.iter().map(|(c, _)| c.to_string()).collect(),
            limit: LIMIT_CUES.iter().map(|(c, _)| c.to_string()).collect(),
            generic: CUES.iter().map(|(c, _)| c.to_string()).collect(),
            words: LIMIT_WORDS.iter().map(|w| w.to_string()).collect(),
            phrases: LIMIT_PHRASES.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// The lists before the hand refinement of 2026-09-30, which added eight
    /// pushback cues, capitalised CAN and COULD, and three "is there no way"
    /// phrases.
    pub fn before_refinement() -> Self {
        let mut c = Self::shipped();
        let added_generic = [
            "give pushback", "some pushback", "pushback to", "pushback on", "pushback that",
            "push back on", "push back that", "want to push back",
        ];
        c.generic.retain(|g| !added_generic.contains(&g.as_str()));
        c.emphasis.retain(|e| e != " CAN " && e != " COULD ");
        c.phrases.retain(|p| !["is there not a way", "isn't there a way", "is there no way"].contains(&p.as_str()));
        c
    }

    /// `corrections::detect`, on these lists.
    pub fn classify(&self, prompt: &str) -> Fire {
        if self.emphasis.iter().any(|c| prompt.contains(c.as_str())) {
            return Fire::Limit;
        }
        let p = prompt.to_lowercase();
        if self.limit.iter().any(|c| p.contains(c.as_str())) {
            return Fire::Limit;
        }
        if !self.generic.iter().any(|c| p.contains(c.as_str())) {
            return Fire::None;
        }
        let talks = p
            .split(|c: char| !c.is_alphanumeric() && c != '\'')
            .any(|w| self.words.iter().any(|x| x == w))
            || self.phrases.iter().any(|x| p.contains(x.as_str()));
        if talks { Fire::Limit } else { Fire::Generic }
    }

    pub fn with(&self, c: &Candidate) -> Self {
        let mut s = self.clone();
        let list = match c.placement {
            Placement::Emphasis => &mut s.emphasis,
            Placement::Limit => &mut s.limit,
            Placement::Generic => &mut s.generic,
            Placement::Phrase => &mut s.phrases,
        };
        list.push(c.text.clone());
        s
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Placement {
    /// Case-sensitive, " WORD ".
    Emphasis,
    /// A limit dispute on its own.
    Limit,
    /// A dispute; a limit dispute when the message talks about limits.
    Generic,
    /// Limit vocabulary, counted only beside a dispute cue.
    Phrase,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Candidate {
    pub placement: Placement,
    pub text: String,
}

impl Candidate {
    pub fn describe(&self) -> String {
        let kind = match self.placement {
            Placement::Emphasis => "emphasis",
            Placement::Limit => "limit cue",
            Placement::Generic => "dispute cue",
            Placement::Phrase => "limit word beside a dispute",
        };
        format!("{kind} {:?}", self.text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    /// Disputes a limit.
    Limit,
    /// Disputes something else.
    Dispute,
    /// Disputes nothing.
    None,
}

impl Label {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "limit" => Some(Self::Limit),
            "dispute" => Some(Self::Dispute),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Is this fire the right answer for this prompt? A generic reminder on a
/// limit dispute is weaker than the audit, but it is not wrong.
fn right(fire: Fire, label: Label) -> bool {
    matches!(
        (fire, label),
        (Fire::Limit, Label::Limit) | (Fire::Generic, Label::Dispute) | (Fire::Generic, Label::Limit) | (Fire::None, Label::None)
    )
}

pub struct Corpus {
    pub prompts: Vec<(String, String)>,
    lower: Vec<String>,
}

/// One `{"ts", "text"}` object per line; a prompt's id is its line number.
pub fn load_corpus(path: &Path) -> Result<Corpus> {
    let text = std::fs::read_to_string(path).with_context(|| format!("no corpus at {}", path.display()))?;
    let prompts: Vec<(String, String)> = text
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap_or(Value::Null);
            (v["ts"].as_str().unwrap_or("").to_string(), v["text"].as_str().unwrap_or("").to_string())
        })
        .collect();
    let lower = prompts.iter().map(|(_, t)| t.to_lowercase()).collect();
    Ok(Corpus { prompts, lower })
}

/// `{"labels": {"<line>": "limit" | "dispute" | "none", ...}}`
pub fn load_labels(path: &Path) -> Result<HashMap<usize, Label>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("no labels at {}", path.display()))?)?;
    Ok(v["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, l)| Some((k.parse().ok()?, Label::parse(l.as_str()?)?)))
        .collect())
}

#[derive(Debug, Default)]
pub struct Eval {
    /// Labelled prompts it turns right.
    pub fixed: Vec<usize>,
    /// Labelled prompts it turns wrong.
    pub broke: Vec<usize>,
    /// Prompts it changes that nobody has labelled.
    pub unlabelled: Vec<usize>,
}

/// What adding `c` changes. Only prompts containing its text can change.
pub fn evaluate(corpus: &Corpus, labels: &HashMap<usize, Label>, base: &CueSet, c: &Candidate) -> Eval {
    let with = base.with(c);
    let needle = c.text.to_lowercase();
    let mut e = Eval::default();
    for (i, (_, text)) in corpus.prompts.iter().enumerate() {
        let may = if c.placement == Placement::Emphasis { text.contains(&c.text) } else { corpus.lower[i].contains(&needle) };
        if !may {
            continue;
        }
        let (before, after) = (base.classify(text), with.classify(text));
        if before == after {
            continue;
        }
        match labels.get(&i) {
            Some(&l) if right(after, l) && !right(before, l) => e.fixed.push(i),
            Some(&l) if !right(after, l) => e.broke.push(i),
            Some(_) => {}
            None => e.unlabelled.push(i),
        }
    }
    e
}

const STOP: &[&str] = &[
    "a", "an", "the", "i", "i'm", "you", "we", "it", "it's", "that", "this", "is", "are", "was", "be",
    "to", "of", "and", "or", "in", "on", "for", "with", "as", "at", "by", "so", "but", "if", "do",
    "my", "our", "your", "me", "us", "there", "what", "which", "just", "also", "not", "no",
];

/// Candidates from one miss: its capitalised words as emphasis, and its word
/// n-grams (1-4) in each list.
pub fn candidates(trigger: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for w in trigger.split(|c: char| !c.is_alphanumeric()) {
        if w.len() >= 2 && w.chars().all(|c| c.is_ascii_uppercase()) {
            out.push(Candidate { placement: Placement::Emphasis, text: format!(" {w} ") });
        }
    }
    let lower = trigger.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| !w.is_empty()).collect();
    for n in 1..=4 {
        for win in words.windows(n) {
            if win.iter().all(|w| STOP.contains(w)) || (n == 1 && win[0].len() < 5) {
                continue;
            }
            let text = win.join(" ");
            for placement in [Placement::Limit, Placement::Generic, Placement::Phrase] {
                out.push(Candidate { placement, text: text.clone() });
            }
        }
    }
    out.sort_by(|a, b| (a.placement, &a.text).cmp(&(b.placement, &b.text)));
    out.dedup();
    out
}

/// Where a candidate stands after replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Standing {
    /// Fixes its miss AND another labelled prompt, breaks none: evidence it generalises.
    Promotable,
    /// Fixes only its own miss, breaks none. No evidence either way: a phrase
    /// unique to one message passes every replay and will never fire again.
    OnlyItsTrigger,
    /// Breaks none of the labelled prompts, but changes unlabelled ones.
    WaitsOnLabels,
    /// Turns a labelled prompt wrong.
    Breaks,
}

#[derive(Debug)]
pub struct Verdict {
    pub candidate: Candidate,
    pub eval: Eval,
    pub standing: Standing,
    /// Promotable: the replay gate passed with evidence beyond the trigger.
    pub accepted: bool,
}

/// Every candidate from the miss at `trigger` that fixes it, judged by the gate.
pub fn mine(corpus: &Corpus, labels: &HashMap<usize, Label>, base: &CueSet, trigger: usize) -> Vec<Verdict> {
    let mut out: Vec<Verdict> = candidates(&corpus.prompts[trigger].1)
        .into_iter()
        .map(|c| {
            let eval = evaluate(corpus, labels, base, &c);
            let standing = if !eval.broke.is_empty() {
                Standing::Breaks
            } else if !eval.unlabelled.is_empty() {
                Standing::WaitsOnLabels
            } else if eval.fixed.iter().any(|&i| i != trigger) {
                Standing::Promotable
            } else {
                Standing::OnlyItsTrigger
            };
            Verdict { candidate: c, eval, accepted: standing == Standing::Promotable, standing }
        })
        .filter(|v| v.eval.fixed.contains(&trigger))
        .collect();
    // Best standing first, then the most general (fewest words): a general cue
    // is the one that can fire again.
    out.sort_by_key(|v| (v.standing, v.candidate.text.split(' ').count(), std::cmp::Reverse(v.eval.fixed.len())));
    out
}

/// Labelled prompts the set gets wrong because it stays silent: what to learn from.
pub fn misses(corpus: &Corpus, labels: &HashMap<usize, Label>, cues: &CueSet) -> Vec<usize> {
    let mut m: Vec<usize> = labels
        .iter()
        .filter(|(&i, &l)| l != Label::None && i < corpus.prompts.len() && !right(cues.classify(&corpus.prompts[i].1), l))
        .map(|(&i, _)| i)
        .collect();
    m.sort();
    m
}

/// How the cue set does on the labelled prompts: right and wrong, by label.
pub fn score(corpus: &Corpus, labels: &HashMap<usize, Label>, cues: &CueSet) -> BTreeMap<&'static str, usize> {
    let mut s = BTreeMap::new();
    for (&i, &l) in labels {
        if i >= corpus.prompts.len() {
            continue;
        }
        let ok = right(cues.classify(&corpus.prompts[i].1), l);
        *s.entry(if ok { "right" } else { "wrong" }).or_default() += 1;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPTS: &[&str] = &[
        "So, but I want to give some pushback that with the right safeguards we COULD further automate",
        "I'm open for pushback and other suggestions",
        "is there not a way we can accurately simulate the real gpu performance costs",
        "are you sure that's a hard limit?",
        "that's not right, the file is in src",
        "what level of raytracing IS possible?",
        "carry on with the next step",
    ];

    #[test]
    fn the_owned_set_classifies_exactly_like_the_shipped_detector() {
        let shipped = CueSet::shipped();
        for p in PROMPTS {
            let theirs = match crate::corrections::detect(p) {
                None => Fire::None,
                Some(label) if crate::corrections::is_limit_cue(label) => Fire::Limit,
                Some(_) => Fire::Generic,
            };
            assert_eq!(shipped.classify(p), theirs, "{p}");
        }
    }

    #[test]
    fn before_the_refinement_the_live_miss_was_silent() {
        assert_eq!(CueSet::before_refinement().classify(PROMPTS[0]), Fire::None);
        assert_eq!(CueSet::shipped().classify(PROMPTS[0]), Fire::Limit);
    }

    #[test]
    fn the_gate_rejects_a_cue_that_fires_on_an_invitation() {
        let corpus = Corpus {
            prompts: PROMPTS.iter().map(|p| (String::new(), p.to_string())).collect(),
            lower: PROMPTS.iter().map(|p| p.to_lowercase()).collect(),
        };
        let labels: HashMap<usize, Label> = [
            (0, Label::Limit), (1, Label::None), (2, Label::None), (3, Label::Limit), (4, Label::Dispute), (5, Label::Limit), (6, Label::None),
        ]
        .into_iter()
        .collect();
        let base = CueSet::before_refinement();
        let verdicts = mine(&corpus, &labels, &base, 0);
        let find = |p: Placement, t: &str| verdicts.iter().find(|v| v.candidate.placement == p && v.candidate.text == t);
        let bare = find(Placement::Generic, "pushback").expect("bare pushback proposed");
        assert!(!bare.accepted && bare.eval.broke == vec![1], "{:?}", bare.eval);
        assert_eq!(bare.standing, Standing::Breaks);
        // Neither fires anywhere else in this corpus: no evidence it generalises.
        assert_eq!(find(Placement::Emphasis, " COULD ").unwrap().standing, Standing::OnlyItsTrigger);
        assert_eq!(find(Placement::Generic, "give some pushback").unwrap().standing, Standing::OnlyItsTrigger);
        assert_eq!(misses(&corpus, &labels, &base), vec![0]);
    }
}
