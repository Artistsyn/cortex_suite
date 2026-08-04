use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::model::DeltaEntry;

#[derive(Debug, Clone)]
pub struct DeltaOptions {
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub max_files: usize,
    pub max_patch_lines: usize,
}

impl Default for DeltaOptions {
    fn default() -> Self {
        Self {
            include: None,
            exclude: None,
            max_files: 256,
            max_patch_lines: 40,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileDelta {
    pub path: String,
    pub status: DeltaStatus,
    pub summary: String,
    pub patch_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

pub fn head_deltas(repo_root: &Path) -> Result<Vec<FileDelta>> {
    head_deltas_with_options(repo_root, &DeltaOptions::default())
}

pub fn commit_deltas(repo_root: &Path, from: &str, to: &str) -> Result<Vec<FileDelta>> {
    commit_deltas_with_options(repo_root, from, to, &DeltaOptions::default())
}

pub fn head_deltas_with_options(repo_root: &Path, opts: &DeltaOptions) -> Result<Vec<FileDelta>> {
    diff_name_status(repo_root, &["diff", "HEAD", "--name-status"], opts)
}

pub fn commit_deltas_with_options(
    repo_root: &Path,
    from: &str,
    to: &str,
    opts: &DeltaOptions,
) -> Result<Vec<FileDelta>> {
    let range = format!("{}..{}", from, to);
    diff_name_status(repo_root, &["diff", &range, "--name-status"], opts)
}

pub fn compress_delta(d: &FileDelta) -> DeltaEntry {
    DeltaEntry {
        path: d.path.clone(),
        change: d.status.as_change_str().to_string(),
        summary: d.summary.clone(),
    }
}

impl DeltaStatus {
    fn as_change_str(self) -> &'static str {
        match self {
            DeltaStatus::Added => "added",
            DeltaStatus::Modified => "modified",
            DeltaStatus::Deleted => "removed",
            DeltaStatus::Renamed => "renamed",
        }
    }
}

fn diff_name_status(repo_root: &Path, args: &[&str], opts: &DeltaOptions) -> Result<Vec<FileDelta>> {
    let out = run_git(repo_root, args)?;
    if out.is_empty() {
        return Ok(vec![]);
    }

    let mut deltas = Vec::new();

    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let mut parts = line.split('\t');
        let status_raw = parts.next().unwrap_or("").trim();
        let path = parts.next().unwrap_or("").trim();
        let renamed_to = parts.next().unwrap_or("").trim();

        if status_raw.is_empty() {
            continue;
        }

        let status = parse_status(status_raw);
        let final_path = if status == DeltaStatus::Renamed && !renamed_to.is_empty() {
            renamed_to.to_string()
        } else {
            path.to_string()
        };

        if final_path.is_empty() || !path_allowed(final_path.as_str(), opts) {
            continue;
        }

        let patch_lines = collect_patch_lines(repo_root, &status, &final_path, opts.max_patch_lines)?;
        let summary = summarize(&status, &patch_lines);

        deltas.push(FileDelta {
            path: final_path,
            status,
            summary,
            patch_lines,
        });
    }

    if deltas.len() > opts.max_files {
        deltas.truncate(opts.max_files);
    }

    Ok(deltas)
}

fn parse_status(status: &str) -> DeltaStatus {
    let lead = status.chars().next().unwrap_or('M');
    match lead {
        'A' => DeltaStatus::Added,
        'D' => DeltaStatus::Deleted,
        'R' => DeltaStatus::Renamed,
        _ => DeltaStatus::Modified,
    }
}

fn collect_patch_lines(
    repo_root: &Path,
    status: &DeltaStatus,
    path: &str,
    max_patch_lines: usize,
) -> Result<Vec<String>> {
    match status {
        DeltaStatus::Deleted => {
            let out = run_git(repo_root, &["diff", "HEAD", "--", path])?;
            let deleted = out.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
            Ok(vec![format!("- deleted {} lines", deleted)])
        }
        _ => {
            let out = run_git(repo_root, &["diff", "HEAD", "--", path])?;
            let lines = out
                .lines()
                .filter(|l| (l.starts_with('+') || l.starts_with('-'))
                    && !l.starts_with("+++")
                    && !l.starts_with("---")
                    && !l.starts_with("@@"))
                .take(max_patch_lines)
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            Ok(lines)
        }
    }
}

fn path_allowed(path: &str, opts: &DeltaOptions) -> bool {
    if let Some(include) = &opts.include {
        if !include.trim().is_empty() && !path.contains(include) {
            return false;
        }
    }

    if let Some(exclude) = &opts.exclude {
        if !exclude.trim().is_empty() && path.contains(exclude) {
            return false;
        }
    }

    true
}

fn summarize(status: &DeltaStatus, patch_lines: &[String]) -> String {
    match status {
        DeltaStatus::Deleted => {
            if let Some(first) = patch_lines.first() {
                first.trim_start_matches('-').trim().to_string()
            } else {
                "deleted file content".to_string()
            }
        }
        _ => patch_lines
            .iter()
            .find(|l| l.starts_with('+') && l.len() > 1)
            .map(|l| l.trim_start_matches('+').trim().to_string())
            .or_else(|| patch_lines.first().map(|l| l.trim().to_string()))
            .unwrap_or_else(|| "modified".to_string()),
    }
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return Ok(String::new()),
    };

    if !output.status.success() {
        return Ok(String::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
