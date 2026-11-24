//! `[origin] subjects = "clean"` and `copybera subjects`: a public export can start at a recent commit, because an
//! old commit's subject may name a private term. A squash export's message carries the
//! subject of every walked commit, so a range is chosen by its subjects: they are read before anything is walked, as
//! the workflow's transformations leave them and as the lint reads the destination's message (the private scrub
//! applied first, then each private pattern against each line). A commit the walk would drop is read too, so the
//! check can refuse a subject the squash would not carry, never the other way. Commit ids and pattern numbers are
//! printed, never a subject or a pattern.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use copybera_rules::{squash_subjects, Commit, Workflow};

use crate::git::Git;
use crate::run::{private_patterns_path, resolve, scrub_rules_path, short, validate};
use crate::run::{PrivatePatterns, ScrubRules};

/// The walked commits whose subject matches a private pattern, oldest first, with the patterns' numbers.
pub(crate) struct SubjectFindings {
    pub commits: usize,
    pub dirty: Vec<(String, Vec<usize>)>,
}

impl SubjectFindings {
    /// Each walked commit's subject as the squash would carry it (`squash_subjects`), then the private scrub, then
    /// each private pattern against each line, as the lint reads the destination's message.
    pub fn read(
        workflow: &Workflow,
        headers: &[Commit],
        private: &PrivatePatterns,
        scrub: Option<&ScrubRules>,
    ) -> Self {
        let subjects = squash_subjects(workflow, headers);
        let dirty = headers
            .iter()
            .zip(subjects)
            .filter_map(|(commit, subject)| {
                let subject = scrub.map_or(subject.clone(), |scrub| scrub.redact(&subject));
                let hits: Vec<usize> = private
                    .regexes
                    .iter()
                    .enumerate()
                    .filter(|(_, regex)| subject.lines().any(|line| regex.is_match(line)))
                    .map(|(index, _)| index + 1)
                    .collect();
                (!hits.is_empty()).then(|| (commit.id.clone(), hits))
            })
            .collect();
        Self {
            commits: headers.len(),
            dirty,
        }
    }

    /// One row per pattern (the number of subjects it matches), the newest dirty commit and the start that leaves
    /// every dirty subject out.
    fn report(&self, out: &mut dyn Write) -> Result<()> {
        let mut by_pattern: BTreeMap<usize, usize> = BTreeMap::new();
        for (_, hits) in &self.dirty {
            for pattern in hits {
                *by_pattern.entry(*pattern).or_default() += 1;
            }
        }
        for (pattern, count) in by_pattern {
            writeln!(
                out,
                "private pattern {pattern}: {count} subject{}",
                if count == 1 { "" } else { "s" }
            )?;
        }
        if let Some((newest, _)) = self.dirty.last() {
            writeln!(out, "newest dirty subject: commit {}", short(newest))?;
            writeln!(
                out,
                "the earliest clean start: [origin] since = \"{newest}\" (the walk starts after it)"
            )?;
        }
        Ok(())
    }
}

fn range_label(since: Option<&str>, until: &str) -> String {
    match since {
        Some(since) => format!("{}..{}", short(since), short(until)),
        None => format!("the root..{}", short(until)),
    }
}

/// Migrate's check: refuses when a walked commit's subject matches a private pattern.
pub(crate) fn require_clean(
    workflow_path: &Path,
    workflow: &Workflow,
    headers: &[Commit],
    since: Option<&str>,
    until: &str,
) -> Result<()> {
    let found = read_findings(workflow_path, workflow, headers)?;
    if found.dirty.is_empty() {
        eprintln!(
            "origin.subjects: {} subject{} in {} clean",
            found.commits,
            if found.commits == 1 { "" } else { "s" },
            range_label(since, until)
        );
        return Ok(());
    }
    let mut report = Vec::new();
    found.report(&mut report)?;
    Err(anyhow!(
        "origin.subjects: {} of {} subjects in {} match a private pattern\n{}",
        found.dirty.len(),
        found.commits,
        range_label(since, until),
        String::from_utf8_lossy(&report).trim_end()
    ))
}

fn read_findings(
    workflow_path: &Path,
    workflow: &Workflow,
    headers: &[Commit],
) -> Result<SubjectFindings> {
    let file = private_patterns_path(workflow_path, workflow).ok_or_else(|| {
        anyhow!("origin.subjects: reading the subjects needs [lint] private (the private-patterns file)")
    })?;
    let private = PrivatePatterns::load(&file)?;
    let scrub = scrub_rules_path(workflow_path, workflow)
        .as_deref()
        .map(ScrubRules::load)
        .transpose()?;
    Ok(SubjectFindings::read(
        workflow,
        headers,
        &private,
        scrub.as_ref(),
    ))
}

/// `copybera subjects WORKFLOW [--since REV]`: the subjects of the range a migrate would walk (the workflow's
/// `origin.since`, or `--since` to try another start), clean or not; dirty refuses.
pub(crate) fn run(
    workflow_path: &Path,
    since: Option<&str>,
    git: &Git,
    out: &mut dyn Write,
) -> Result<()> {
    let workflow = validate(workflow_path)?;
    let base = workflow_path.parent().unwrap_or_else(|| Path::new("."));
    let origin = workflow
        .origin
        .location
        .path
        .as_deref()
        .map(|path| resolve(base, path))
        .ok_or_else(|| anyhow!("origin.path: subjects reads a local checkout"))?;
    let until = git
        .rev_parse(&origin, &workflow.origin.ref_name)
        .with_context(|| format!("origin.ref: cannot resolve {}", workflow.origin.ref_name))?;
    let since = match since.or(workflow.origin.since.as_deref()) {
        Some(rev) => Some(
            git.rev_parse(&origin, rev)
                .with_context(|| format!("since: cannot resolve {rev}"))?,
        ),
        None => None,
    };
    let ids = git.rev_list(&origin, since.as_deref(), &until)?;
    let headers = git.load_commit_headers(&origin, &ids)?;
    let found = read_findings(workflow_path, &workflow, &headers)?;
    writeln!(
        out,
        "subjects: {} commit{} in {} (first parent)",
        found.commits,
        if found.commits == 1 { "" } else { "s" },
        range_label(since.as_deref(), &until)
    )?;
    found.report(out)?;
    if found.dirty.is_empty() {
        writeln!(out, "ok: subjects clean")?;
        Ok(())
    } else {
        Err(anyhow!(
            "subjects: {} of {} subjects match a private pattern",
            found.dirty.len(),
            found.commits
        ))
    }
}
