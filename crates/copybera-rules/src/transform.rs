use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regex::Regex;

use crate::globmatch::path_allowed;
use crate::strip::strip_references;
use crate::workflow::{Authoring, Identity, Mode, ReplaceOn, Scope, Transformation, Workflow};

/// A file's bytes, shared: the commits of a window hold one copy of a file they all carry, and a transformation that
/// leaves a file alone keeps its copy (a copy per commit of a window of 32 commits of a 150 MB tree
/// is several gigabytes).
pub type Blob = Arc<[u8]>;

/// Bytes, paths, and messages the CLI crate supplies. The rules crate does not call git.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub author_date: Option<String>,
    pub files: BTreeMap<String, Blob>,
    /// Git modes other than `100644` (`100755`, `120000`), by path. A moved path takes its mode along; a
    /// stripped path drops it.
    pub modes: BTreeMap<String, String>,
}

/// The CLI implements this. A stub that always returns `None` selects the deterministic fallback.
pub trait MessageRewriter {
    fn rewrite(&self, message: &str, budget_tokens: u32) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripEvent {
    pub transform: String,
    pub commit: String,
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drop {
    pub commit: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    pub commit: String,
    pub original_subject: String,
    pub rewritten_subject: String,
}

#[derive(Debug, Clone)]
pub struct TransformResult {
    pub commits: Vec<Commit>,
    pub stripped: Vec<StripEvent>,
    pub drops: Vec<Drop>,
    pub rewritten: Vec<Rewritten>,
    pub files_stripped: BTreeSet<String>,
    /// Punctuation a stripped reference left behind (`()`, `(,`, `; ;`, `owner=`), by commit and
    /// path. The CLI refuses the run on any.
    pub refused: Vec<StripEvent>,
}

pub fn apply(
    workflow: &Workflow,
    commits: Vec<Commit>,
    parent_files: BTreeMap<String, Blob>,
    rewriter: &dyn MessageRewriter,
) -> TransformResult {
    apply_window(
        workflow,
        commits,
        parent_files,
        rewriter,
        &mut WindowState::default(),
        true,
    )
}

/// The squash of an already transformed walk: the author, the trailers and the joined subjects of every commit, the
/// last commit's tree. A walk applied in windows (`apply_window` with `finish` false) ends here.
pub fn finish_squash(workflow: &Workflow, commits: Vec<Commit>) -> Vec<Commit> {
    let reference_regexes = workflow.reference_regexes();
    squash(commits, &workflow.authoring, &reference_regexes)
}

/// The subject each of `commits` gives a squash: its message as the workflow's transformations leave it (the files
/// aside, and every commit read as kept), its first line, the squash's reference strip applied (a range's
/// subjects are read before the range is walked). A name-following `replace_pairs` reads no moved names here.
pub fn squash_subjects(workflow: &Workflow, commits: &[Commit]) -> Vec<String> {
    let reference_regexes = workflow.reference_regexes();
    let (mut stripped, mut refused) = (Vec::new(), Vec::new());
    commits
        .iter()
        .map(|header| {
            let mut commit = Commit {
                id: header.id.clone(),
                message: header.message.clone(),
                ..Commit::default()
            };
            for transform in &workflow.transformations {
                match transform {
                    Transformation::StripReferences { regexes, .. } => {
                        strip_commit_references(
                            &mut commit,
                            regexes,
                            "",
                            &mut stripped,
                            &mut refused,
                        );
                    }
                    Transformation::RewriteMessages {
                        hook,
                        prompt_budget_tokens,
                        ..
                    } => {
                        commit.message = rewrite_message(
                            &commit.message,
                            *prompt_budget_tokens,
                            hook.is_some(),
                            &StubRewriter,
                            &reference_regexes,
                        );
                    }
                    Transformation::ReplaceText {
                        replacement,
                        on,
                        regex,
                        scope,
                        ..
                    } => replace_text(
                        &mut commit,
                        regex,
                        replacement,
                        *on,
                        scope,
                        "",
                        &mut stripped,
                    ),
                    Transformation::ReplacePairs {
                        pairs,
                        on,
                        scope,
                        follow_moves,
                    } => {
                        let pairs = with_moved_names(pairs, *follow_moves, &BTreeMap::new());
                        replace_pairs(&mut commit, &pairs, *on, scope, "", &mut stripped);
                    }
                    _ => {}
                }
                stripped.clear();
                refused.clear();
            }
            strip_references(first_line(&commit.message), &reference_regexes).text
        })
        .collect()
}

/// What a walk applied in windows carries from one window to the next.
#[derive(Debug, Clone, Default)]
pub struct WindowState {
    /// The name pairs each `move_paths` transformation (by index) recorded over the commits walked so far.
    pub moved: BTreeMap<usize, BTreeMap<String, String>>,
    /// When set, the names every `move_paths` transformation records over the whole walk (a first pass found them):
    /// `follow_moves` reads these, so a message in an early window is rewritten as a one-pass walk rewrites it.
    pub seed: Option<BTreeMap<usize, BTreeMap<String, String>>>,
    /// For each `drop_empty_commits` transformation (by index), the tree of the last commit it kept, as the
    /// transformations before it left it: the next window's first commit is compared with it.
    pub drop_baselines: BTreeMap<usize, BTreeMap<String, Blob>>,
    /// For each editing transformation (by index), what it did to the last commit it ran over.
    pub edit_memos: BTreeMap<usize, EditMemo>,
    /// For each `strip_matching` transformation (by index), the verdict on each file of the last commit it read.
    pub strip_memos: BTreeMap<usize, StripMemo>,
}

impl WindowState {
    /// The names the `move_paths` transformations before `index` record.
    fn names_before(&self, index: usize) -> BTreeMap<String, String> {
        let source = self.seed.as_ref().unwrap_or(&self.moved);
        let mut names = BTreeMap::new();
        for recorded in source.range(..index).map(|(_, names)| names) {
            names.extend(recorded.iter().map(|(from, to)| (from.clone(), to.clone())));
        }
        names
    }
}

/// `apply` over one window of a walk: `parent_files` is the raw tree before
/// the window's first commit, `state` carries the moved names and the drop baselines of the windows before, and
/// `finish` false leaves the squash (or the author mapping) to the caller, so a squash walk keeps one window of trees
/// in memory instead of every commit's.
pub fn apply_window(
    workflow: &Workflow,
    mut commits: Vec<Commit>,
    parent_files: BTreeMap<String, Blob>,
    rewriter: &dyn MessageRewriter,
    state: &mut WindowState,
    finish: bool,
) -> TransformResult {
    let mut stripped = Vec::new();
    let mut drops = Vec::new();
    let mut rewritten = Vec::new();
    let mut files_stripped = BTreeSet::new();
    let mut refused = Vec::new();
    let original_subjects: BTreeMap<String, String> = commits
        .iter()
        .map(|c| (c.id.clone(), first_line(&c.message).to_string()))
        .collect();

    filter_paths(
        &mut commits,
        &workflow.origin_paths.include_set,
        &workflow.origin_paths.exclude_set,
        workflow.origin_paths.include.is_empty(),
        "paths.origin",
        &mut stripped,
        &mut files_stripped,
    );
    drop_empty(
        &mut commits,
        &mut drops,
        "path filter",
        &mut BTreeMap::new(),
        false,
    );

    let reference_regexes = workflow.reference_regexes();

    for (index, transform) in workflow.transformations.iter().enumerate() {
        let label = format!("transformations[{index}].{}", transform.type_name());
        match transform {
            Transformation::StripPaths { matcher, .. } => {
                for commit in &mut commits {
                    let before: Vec<String> = commit.files.keys().cloned().collect();
                    commit.files.retain(|path, _| !matcher.is_match(path));
                    for path in before {
                        if !commit.files.contains_key(&path) {
                            files_stripped.insert(path.clone());
                            stripped.push(StripEvent {
                                transform: label.clone(),
                                commit: commit.id.clone(),
                                path,
                                detail: "path dropped".to_string(),
                            });
                        }
                    }
                }
            }
            Transformation::StripReferences { regexes, .. } => {
                let mut memo = state.edit_memos.remove(&index).unwrap_or_default();
                edit_each(
                    &mut commits,
                    &mut memo,
                    &mut stripped,
                    &mut refused,
                    |commit, stripped, refused| {
                        strip_commit_references(commit, regexes, &label, stripped, refused);
                    },
                );
                state.edit_memos.insert(index, memo);
            }
            Transformation::RewriteMessages {
                hook,
                prompt_budget_tokens,
                ..
            } => {
                for commit in &mut commits {
                    let original_subject = original_subjects
                        .get(&commit.id)
                        .cloned()
                        .unwrap_or_else(|| first_line(&commit.message).to_string());
                    let rewritten_subject = rewrite_message(
                        &commit.message,
                        *prompt_budget_tokens,
                        hook.is_some(),
                        rewriter,
                        &reference_regexes,
                    );
                    rewritten.push(Rewritten {
                        commit: commit.id.clone(),
                        original_subject,
                        rewritten_subject: first_line(&rewritten_subject).to_string(),
                    });
                    commit.message = rewritten_subject;
                }
            }
            Transformation::ReplaceText {
                replacement,
                on,
                regex,
                scope,
                ..
            } => {
                let mut memo = state.edit_memos.remove(&index).unwrap_or_default();
                edit_each(
                    &mut commits,
                    &mut memo,
                    &mut stripped,
                    &mut refused,
                    |commit, stripped, _| {
                        replace_text(commit, regex, replacement, *on, scope, &label, stripped);
                    },
                );
                state.edit_memos.insert(index, memo);
            }
            Transformation::ReplacePairs {
                pairs,
                on,
                scope,
                follow_moves,
            } => {
                let pairs = with_moved_names(pairs, *follow_moves, &state.names_before(index));
                // The pairs a name-following rewrite reads grow as the walk records moves: its results are kept for
                // one window only.
                let mut memo = if *follow_moves {
                    EditMemo::default()
                } else {
                    state.edit_memos.remove(&index).unwrap_or_default()
                };
                edit_each(
                    &mut commits,
                    &mut memo,
                    &mut stripped,
                    &mut refused,
                    |commit, stripped, _| {
                        replace_pairs(commit, &pairs, *on, scope, &label, stripped);
                    },
                );
                if !*follow_moves {
                    state.edit_memos.insert(index, memo);
                }
            }
            Transformation::StripMatching {
                regexes,
                scope,
                private_regexes,
                ..
            } => {
                let mut memo = state.strip_memos.remove(&index).unwrap_or_default();
                for commit in &mut commits {
                    strip_matching(
                        commit,
                        (regexes, private_regexes),
                        scope,
                        &label,
                        (&mut stripped, &mut files_stripped),
                        &mut memo,
                    );
                }
                state.strip_memos.insert(index, memo);
            }
            Transformation::MovePaths { pairs, scope } => {
                let mut names = BTreeMap::new();
                for commit in &mut commits {
                    move_paths(commit, pairs, scope, &label, &mut stripped, &mut names);
                }
                state.moved.entry(index).or_default().extend(names);
            }
            Transformation::RenamePaths { from, to } => {
                for commit in &mut commits {
                    rename_paths(commit, from, to, &label, &mut stripped);
                }
            }
            Transformation::StripComments {
                languages,
                keep_docs,
                scope,
            } => {
                let mut memo = state.edit_memos.remove(&index).unwrap_or_default();
                edit_each(
                    &mut commits,
                    &mut memo,
                    &mut stripped,
                    &mut refused,
                    |commit, stripped, _| {
                        strip_comments(commit, languages, *keep_docs, scope, &label, stripped);
                    },
                );
                state.edit_memos.insert(index, memo);
            }
            Transformation::DropEmptyCommits => {
                let mut baseline = state
                    .drop_baselines
                    .remove(&index)
                    .unwrap_or_else(|| transformed_parent(workflow, &parent_files, index));
                drop_empty(
                    &mut commits,
                    &mut drops,
                    "empty after strip",
                    &mut baseline,
                    true,
                );
                state.drop_baselines.insert(index, baseline);
            }
        }
    }

    filter_paths(
        &mut commits,
        &workflow.destination_paths.include_set,
        &workflow.destination_paths.exclude_set,
        workflow.destination_paths.include.is_empty(),
        "paths.destination",
        &mut stripped,
        &mut files_stripped,
    );

    for commit in &mut commits {
        let files = &commit.files;
        commit.modes.retain(|path, _| files.contains_key(path));
    }

    if !finish {
        // The caller squashes the windows together.
    } else if workflow.mode == Mode::Squash {
        commits = squash(commits, &workflow.authoring, &reference_regexes);
    } else {
        for commit in &mut commits {
            let (name, email) = map_author(
                &workflow.authoring,
                &commit.author_name,
                &commit.author_email,
            );
            commit.author_name = name;
            commit.author_email = email;
            commit.message = rewrite_trailers(&commit.message, &workflow.authoring);
        }
    }

    TransformResult {
        commits,
        stripped,
        drops,
        rewritten,
        files_stripped,
        refused,
    }
}

/// What an editing transformation did to the last commit it ran over: that commit's files before and after, and
/// the events it recorded per path. The next commit's files that are the same blobs take these results instead of
/// being edited again (editing a large tree whose files mostly do not change from one commit to the next costs
/// seconds a commit). The editing transformations read each file on its own (its path and bytes),
/// so the same blob at the same path gives the same result.
#[derive(Debug, Clone, Default)]
pub struct EditMemo {
    inputs: BTreeMap<String, Blob>,
    outputs: BTreeMap<String, Blob>,
    stripped: BTreeMap<String, Vec<StripEvent>>,
    refused: BTreeMap<String, Vec<StripEvent>>,
}

const MESSAGE_PATH: &str = "<message>";

/// The events of one commit in the order an edit over the whole commit records them: the message's first, then the
/// files' in path order.
fn commit_order(events: &mut [StripEvent]) {
    events
        .sort_by(|a, b| (a.path != MESSAGE_PATH, &a.path).cmp(&(b.path != MESSAGE_PATH, &b.path)));
}

fn events_by_path(events: &[StripEvent]) -> BTreeMap<String, Vec<StripEvent>> {
    let mut by_path: BTreeMap<String, Vec<StripEvent>> = BTreeMap::new();
    for event in events.iter().filter(|event| event.path != MESSAGE_PATH) {
        by_path
            .entry(event.path.clone())
            .or_default()
            .push(event.clone());
    }
    by_path
}

fn replay(events: Option<&Vec<StripEvent>>, commit: &str, into: &mut Vec<StripEvent>) {
    for event in events.into_iter().flatten() {
        into.push(StripEvent {
            commit: commit.to_string(),
            ..event.clone()
        });
    }
}

/// Runs `edit` (a transformation that rewrites files' bytes and never adds or removes a file) over each commit in
/// order. A file that is the blob `memo` saw at that path takes the result recorded there, events included, and is
/// not edited again; an edited file that comes out with the bytes the commit before holds at that path takes that
/// commit's copy. So a window's commits hold one copy of a file they carry and edit alike, and an unchanged file is
/// edited once.
fn edit_each(
    commits: &mut [Commit],
    memo: &mut EditMemo,
    stripped: &mut Vec<StripEvent>,
    refused: &mut Vec<StripEvent>,
    mut edit: impl FnMut(&mut Commit, &mut Vec<StripEvent>, &mut Vec<StripEvent>),
) {
    for index in 0..commits.len() {
        let (before, rest) = commits.split_at_mut(index);
        let commit = &mut rest[0];
        let inputs = commit.files.clone();
        let mut carried = Vec::new();
        commit.files.retain(|path, blob| {
            let known = memo
                .inputs
                .get(path)
                .is_some_and(|held| same_blob(held, blob))
                && memo.outputs.contains_key(path);
            if known {
                carried.push(path.clone());
            }
            !known
        });
        let (mut new_stripped, mut new_refused) = (Vec::new(), Vec::new());
        edit(commit, &mut new_stripped, &mut new_refused);
        if let Some(previous) = before.last() {
            share_alike(&previous.files, &mut commit.files);
        }
        for path in carried {
            replay(memo.stripped.get(&path), &commit.id, &mut new_stripped);
            replay(memo.refused.get(&path), &commit.id, &mut new_refused);
            let output = Arc::clone(&memo.outputs[&path]);
            commit.files.insert(path, output);
        }
        commit_order(&mut new_stripped);
        commit_order(&mut new_refused);
        *memo = EditMemo {
            inputs,
            outputs: commit.files.clone(),
            stripped: events_by_path(&new_stripped),
            refused: events_by_path(&new_refused),
        };
        stripped.extend(new_stripped);
        refused.extend(new_refused);
    }
}

/// The bytes some commits' files hold and the number of distinct blobs holding them: a blob several commits share
/// counts once. A squash walk prints it per window.
pub fn held_blobs(commits: &[Commit]) -> (usize, usize) {
    let mut seen = BTreeSet::new();
    let mut bytes = 0;
    for blob in commits.iter().flat_map(|commit| commit.files.values()) {
        if seen.insert(Arc::as_ptr(blob).cast::<u8>() as usize) {
            bytes += blob.len();
        }
    }
    (bytes, seen.len())
}

fn share_alike(previous: &BTreeMap<String, Blob>, files: &mut BTreeMap<String, Blob>) {
    for (path, blob) in files.iter_mut() {
        if let Some(held) = previous.get(path) {
            if !Arc::ptr_eq(held, blob) && held[..] == blob[..] {
                *blob = Arc::clone(held);
            }
        }
    }
}

/// The parent tree (the raw tree at the walk's start) as the origin path filter and the transformations before
/// `upto` leave it. drop_empty_commits compares the first walked commit with this, as it compares every later
/// commit with the one before it, so a walk resumed at any commit drops what a whole walk drops.
/// Only the files matter: the events belong to no walked commit and are discarded.
fn transformed_parent(
    workflow: &Workflow,
    parent_files: &BTreeMap<String, Blob>,
    upto: usize,
) -> BTreeMap<String, Blob> {
    if parent_files.is_empty() {
        return BTreeMap::new();
    }
    let mut parent = [Commit {
        id: String::new(),
        message: String::new(),
        author_name: String::new(),
        author_email: String::new(),
        author_date: None,
        files: parent_files.clone(),
        modes: BTreeMap::new(),
    }];
    let mut stripped = Vec::new();
    let mut files_stripped = BTreeSet::new();
    let mut refused = Vec::new();
    filter_paths(
        &mut parent,
        &workflow.origin_paths.include_set,
        &workflow.origin_paths.exclude_set,
        workflow.origin_paths.include.is_empty(),
        "paths.origin",
        &mut stripped,
        &mut files_stripped,
    );
    let [mut tree] = parent;
    let mut moved_names = BTreeMap::new();
    for transform in &workflow.transformations[..upto] {
        match transform {
            Transformation::StripPaths { matcher, .. } => {
                tree.files.retain(|path, _| !matcher.is_match(path));
            }
            Transformation::StripReferences { regexes, .. } => {
                strip_commit_references(&mut tree, regexes, "", &mut stripped, &mut refused);
            }
            Transformation::ReplaceText {
                replacement,
                on,
                regex,
                scope,
                ..
            } => replace_text(&mut tree, regex, replacement, *on, scope, "", &mut stripped),
            Transformation::ReplacePairs {
                pairs,
                on,
                scope,
                follow_moves,
            } => {
                let pairs = with_moved_names(pairs, *follow_moves, &moved_names);
                replace_pairs(&mut tree, &pairs, *on, scope, "", &mut stripped);
            }
            Transformation::StripMatching {
                regexes,
                scope,
                private_regexes,
                ..
            } => {
                let mut files_stripped = BTreeSet::new();
                strip_matching(
                    &mut tree,
                    (regexes, private_regexes),
                    scope,
                    "",
                    (&mut stripped, &mut files_stripped),
                    &mut StripMemo::default(),
                );
            }
            Transformation::MovePaths { pairs, scope } => {
                move_paths(&mut tree, pairs, scope, "", &mut stripped, &mut moved_names);
            }
            Transformation::RenamePaths { from, to } => {
                rename_paths(&mut tree, from, to, "", &mut stripped);
            }
            Transformation::StripComments {
                languages,
                keep_docs,
                scope,
            } => strip_comments(&mut tree, languages, *keep_docs, scope, "", &mut stripped),
            // Message rewrites leave the files alone; an earlier drop_empty_commits drops no parent.
            Transformation::RewriteMessages { .. } | Transformation::DropEmptyCommits => {}
        }
    }
    tree.files
}

fn filter_paths(
    commits: &mut [Commit],
    include: &globset::GlobSet,
    exclude: &globset::GlobSet,
    include_empty: bool,
    label: &str,
    stripped: &mut Vec<StripEvent>,
    files_stripped: &mut BTreeSet<String>,
) {
    for commit in commits.iter_mut() {
        let before: Vec<String> = commit.files.keys().cloned().collect();
        commit
            .files
            .retain(|path, _| path_allowed(path, include, exclude, include_empty));
        for path in before {
            if !commit.files.contains_key(&path) {
                files_stripped.insert(path.clone());
                stripped.push(StripEvent {
                    transform: label.to_string(),
                    commit: commit.id.clone(),
                    path,
                    detail: "path filter".to_string(),
                });
            }
        }
    }
}

fn drop_empty(
    commits: &mut Vec<Commit>,
    drops: &mut Vec<Drop>,
    reason: &str,
    previous: &mut BTreeMap<String, Blob>,
    compare_parent: bool,
) {
    let mut kept = Vec::new();
    for commit in commits.drain(..) {
        let empty_tree = commit.files.is_empty();
        let same_as_parent = compare_parent && commit.files == *previous;
        if empty_tree || same_as_parent {
            drops.push(Drop {
                commit: commit.id.clone(),
                reason: if empty_tree && reason == "path filter" {
                    reason.to_string()
                } else if empty_tree {
                    "empty after strip".to_string()
                } else {
                    reason.to_string()
                },
            });
            continue;
        }
        previous.clone_from(&commit.files);
        kept.push(commit);
    }
    *commits = kept;
}

fn strip_commit_references(
    commit: &mut Commit,
    regexes: &[Regex],
    label: &str,
    stripped: &mut Vec<StripEvent>,
    refused: &mut Vec<StripEvent>,
) {
    let message = strip_references(&commit.message, regexes);
    refuse_leftovers(commit, "<message>", &message.leftovers, label, refused);
    let msg_hits = message.hits;
    let message = message.text;
    if msg_hits > 0 {
        stripped.push(StripEvent {
            transform: label.to_string(),
            commit: commit.id.clone(),
            path: "<message>".to_string(),
            detail: format!("{msg_hits} hits"),
        });
        commit.message = message;
    }
    let paths: Vec<String> = commit.files.keys().cloned().collect();
    for path in paths {
        let Some(bytes) = commit.files.get(&path) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            continue;
        };
        let next = strip_references(text, regexes);
        refuse_leftovers(commit, &path, &next.leftovers, label, refused);
        let (next, hits) = (next.text, next.hits);
        if hits > 0 {
            stripped.push(StripEvent {
                transform: label.to_string(),
                commit: commit.id.clone(),
                path: path.clone(),
                detail: format!("{hits} hits"),
            });
            commit.files.insert(path, Blob::from(next.into_bytes()));
        }
    }
}

fn replace_text(
    commit: &mut Commit,
    regex: &Regex,
    replacement: &str,
    on: ReplaceOn,
    scope: &Scope,
    label: &str,
    stripped: &mut Vec<StripEvent>,
) {
    edit_text(commit, on, scope, label, stripped, |piece| {
        let hits = regex.find_iter(piece).count();
        if hits == 0 {
            return (piece.to_string(), 0);
        }
        (regex.replace_all(piece, replacement).into_owned(), hits)
    });
}

fn replace_pairs(
    commit: &mut Commit,
    pairs: &[(String, String)],
    on: ReplaceOn,
    scope: &Scope,
    label: &str,
    stripped: &mut Vec<StripEvent>,
) {
    edit_text(commit, on, scope, label, stripped, |piece| {
        apply_pairs(piece, pairs)
    });
}

/// Every pair in order over `text`, each only over text no earlier pair wrote (copybara's two-phase sentinel
/// replace, without the sentinels). Returns the new text and the number of replacements.
pub(crate) fn apply_pairs(text: &str, pairs: &[(String, String)]) -> (String, usize) {
    if !pairs.iter().any(|(from, _)| text.contains(from.as_str())) {
        return (text.to_string(), 0);
    }
    // (text, written by a pair)
    let mut spans: Vec<(String, bool)> = vec![(text.to_string(), false)];
    let mut hits = 0;
    for (from, to) in pairs {
        let mut next = Vec::with_capacity(spans.len());
        for (span, written) in spans {
            if written || !span.contains(from.as_str()) {
                next.push((span, written));
                continue;
            }
            let mut rest = span.as_str();
            while let Some(at) = rest.find(from.as_str()) {
                if at > 0 {
                    next.push((rest[..at].to_string(), false));
                }
                next.push((to.clone(), true));
                hits += 1;
                rest = &rest[at + from.len()..];
            }
            if !rest.is_empty() {
                next.push((rest.to_string(), false));
            }
        }
        spans = next;
    }
    (spans.into_iter().map(|(span, _)| span).collect(), hits)
}

/// Runs `edit` over the message (when `on` names messages) and every covered UTF-8 file (when `on` names files),
/// through the scope's line guard, and records one event per changed message or file.
fn edit_text(
    commit: &mut Commit,
    on: ReplaceOn,
    scope: &Scope,
    label: &str,
    stripped: &mut Vec<StripEvent>,
    mut edit: impl FnMut(&str) -> (String, usize),
) {
    let mut rewrite = |text: &str| guarded(text, scope, &mut edit);
    if matches!(on, ReplaceOn::Messages | ReplaceOn::Both) {
        let (next, hits) = rewrite(&commit.message);
        if hits > 0 {
            commit.message = next;
            stripped.push(StripEvent {
                transform: label.to_string(),
                commit: commit.id.clone(),
                path: "<message>".to_string(),
                detail: format!("{hits} replacements"),
            });
        }
    }
    if matches!(on, ReplaceOn::Files | ReplaceOn::Both) {
        let paths: Vec<String> = commit
            .files
            .keys()
            .filter(|path| scope.covers(path))
            .cloned()
            .collect();
        for path in paths {
            let Some(bytes) = commit.files.get(&path) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(bytes) else {
                continue;
            };
            let (next, hits) = rewrite(text);
            if hits > 0 {
                commit
                    .files
                    .insert(path.clone(), Blob::from(next.into_bytes()));
                stripped.push(StripEvent {
                    transform: label.to_string(),
                    commit: commit.id.clone(),
                    path,
                    detail: format!("{hits} replacements"),
                });
            }
        }
    }
}

/// Runs `edit` over `text`, or, when the scope names `ignore_lines`, over each line the scope does not ignore (its
/// line ending included); an ignored line is copied as it was. `edit` returns the new text and its hit count.
fn guarded(
    text: &str,
    scope: &Scope,
    mut edit: impl FnMut(&str) -> (String, usize),
) -> (String, usize) {
    if !scope.has_ignore_lines() {
        return edit(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut hits = 0;
    for piece in text.split_inclusive('\n') {
        let line = piece.strip_suffix('\n').unwrap_or(piece);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if scope.ignores(line) {
            out.push_str(piece);
            continue;
        }
        let (next, count) = edit(piece);
        hits += count;
        out.push_str(&next);
    }
    (out, hits)
}

/// The same bytes: the same blob, or a copy of it (a window's loader reads a file the window before held again).
fn same_blob(held: &Blob, blob: &Blob) -> bool {
    Arc::ptr_eq(held, blob) || held[..] == blob[..]
}

/// What `strip_matching` decided about each file of the last commit it read: the blob and the pattern that hit it, if
/// any. A file that is that blob again at that path gets the same verdict without its text being searched again.
#[derive(Debug, Clone, Default)]
pub struct StripMemo {
    verdicts: BTreeMap<String, (Blob, Option<String>)>,
}

#[cfg(test)]
thread_local! {
    /// The files `strip_matching` searched, for the memo's tests.
    static STRIP_SEARCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The first pattern of `regexes` (a whole-text pattern) or `private_regexes` (line patterns, as the lint reads
/// them) that matches a UTF-8 file, by number (1-based), never the text.
fn strip_verdict(bytes: &[u8], regexes: &[Regex], private_regexes: &[Regex]) -> Option<String> {
    #[cfg(test)]
    STRIP_SEARCHES.with(|count| count.set(count.get() + 1));
    let text = std::str::from_utf8(bytes).ok()?;
    if let Some(at) = regexes.iter().position(|regex| regex.is_match(text)) {
        return Some(format!("pattern {}", at + 1));
    }
    let at = private_regexes
        .iter()
        .position(|regex| text.lines().any(|line| regex.is_match(line)))?;
    Some(format!("private pattern {}", at + 1))
}

/// Strips every covered UTF-8 file whose text matches one of `regexes`; the event names the first matching
/// pattern's number (1-based), never the text. A file `memo` saw at that path as the same bytes keeps its verdict.
fn strip_matching(
    commit: &mut Commit,
    (regexes, private_regexes): (&[Regex], &[Regex]),
    scope: &Scope,
    label: &str,
    (stripped, files_stripped): (&mut Vec<StripEvent>, &mut BTreeSet<String>),
    memo: &mut StripMemo,
) {
    let mut verdicts = BTreeMap::new();
    let mut flagged: Vec<(String, String)> = Vec::new();
    for (path, bytes) in commit.files.iter().filter(|(path, _)| scope.covers(path)) {
        let verdict = match memo.verdicts.get(path) {
            Some((held, verdict)) if same_blob(held, bytes) => verdict.clone(),
            _ => strip_verdict(bytes, regexes, private_regexes),
        };
        if let Some(number) = &verdict {
            flagged.push((path.clone(), number.clone()));
        }
        verdicts.insert(path.clone(), (Arc::clone(bytes), verdict));
    }
    memo.verdicts = verdicts;
    for (path, number) in flagged {
        commit.files.remove(&path);
        files_stripped.insert(path.clone());
        stripped.push(StripEvent {
            transform: label.to_string(),
            commit: commit.id.clone(),
            path,
            detail: format!("path dropped ({number})"),
        });
    }
}

/// `strip_comments`: each covered file in a listed language loses its comments; the event names the
/// language and what went (comments, whole lines, bytes).
fn strip_comments(
    commit: &mut Commit,
    languages: &[crate::comments::Lang],
    keep_docs: bool,
    scope: &Scope,
    label: &str,
    stripped: &mut Vec<StripEvent>,
) {
    let paths: Vec<String> = commit
        .files
        .keys()
        .filter(|path| scope.covers(path))
        .cloned()
        .collect();
    for path in paths {
        let Some(lang) = crate::comments::Lang::of_path(&path).filter(|l| languages.contains(l))
        else {
            continue;
        };
        let Some(text) = commit
            .files
            .get(&path)
            .and_then(|b| std::str::from_utf8(b).ok())
        else {
            continue;
        };
        let Some((next, stats)) = crate::comments::strip_comments(text, lang, keep_docs) else {
            continue;
        };
        commit
            .files
            .insert(path.clone(), Blob::from(next.into_bytes()));
        stripped.push(StripEvent {
            transform: label.to_string(),
            commit: commit.id.clone(),
            path,
            detail: format!(
                "{} comments, {} lines, {} bytes ({}; {} kept)",
                stats.comments,
                stats.lines,
                stats.bytes,
                lang.name(),
                stats.kept
            ),
        });
    }
}

/// `pairs`, or, with `follow`, the moved names (longest first) ahead of them.
fn with_moved_names(
    pairs: &[(String, String)],
    follow: bool,
    moved_names: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    if !follow {
        return pairs.to_vec();
    }
    let mut names: Vec<(String, String)> = moved_names
        .iter()
        .map(|(from, to)| (from.clone(), to.clone()))
        .collect();
    names.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    names.extend(pairs.iter().cloned());
    names
}

/// A file name without its last extension (`Probe.swift` -> `Probe`; a dot file keeps its name).
fn stem(segment: &str) -> &str {
    match segment.rfind('.') {
        Some(at) if at > 0 => &segment[..at],
        _ => segment,
    }
}

/// Moves every covered path whose segments the pairs rewrite, except where the new name is taken (by a path in the
/// tree or by a second move): those stay, one event each. Records each moved segment's name pair in `moved_names`
/// for replace_pairs `follow_moves`, except a bare pair name (`acme/` is a folder, not a word to rewrite in prose)
/// and a segment that also names a path that stayed (its references must keep the old name).
fn move_paths(
    commit: &mut Commit,
    pairs: &[(String, String)],
    scope: &Scope,
    label: &str,
    stripped: &mut Vec<StripEvent>,
    moved_names: &mut BTreeMap<String, String>,
) {
    let rename = |path: &str| -> String {
        path.split('/')
            .map(|segment| apply_pairs(segment, pairs).0)
            .collect::<Vec<_>>()
            .join("/")
    };
    let wanted: Vec<(String, String)> = commit
        .files
        .keys()
        .filter(|path| scope.covers(path))
        .filter_map(|path| {
            let to = rename(path);
            (to != *path).then(|| (path.clone(), to))
        })
        .collect();
    let mut landing: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, to) in &wanted {
        *landing.entry(to.as_str()).or_default() += 1;
    }
    let taken = |to: &str| commit.files.contains_key(to) || landing[to] > 1;
    let blocked: BTreeSet<&str> = wanted
        .iter()
        .filter(|(_, to)| taken(to))
        .flat_map(|(from, _)| from.split('/'))
        .filter(|segment| apply_pairs(segment, pairs).1 > 0)
        .collect();
    let bare: BTreeSet<String> = pairs.iter().map(|(from, _)| from.to_lowercase()).collect();
    let mut moves = Vec::new();
    for (from, to) in &wanted {
        if taken(to) {
            stripped.push(StripEvent {
                transform: label.to_string(),
                commit: commit.id.clone(),
                path: from.clone(),
                detail: format!("stays: target taken ({to})"),
            });
            continue;
        }
        for (old, new) in from.split('/').zip(to.split('/')) {
            if old == new || blocked.contains(old) {
                continue;
            }
            let (old, new) = (stem(old), stem(new));
            if old == new || bare.contains(&old.trim_matches('.').to_lowercase()) {
                continue;
            }
            moved_names.insert(old.to_string(), new.to_string());
        }
        moves.push((from.clone(), to.clone()));
    }
    for (from, to) in moves {
        if let Some(bytes) = commit.files.remove(&from) {
            if let Some(mode) = commit.modes.remove(&from) {
                commit.modes.insert(to.clone(), mode);
            }
            stripped.push(StripEvent {
                transform: label.to_string(),
                commit: commit.id.clone(),
                path: from,
                detail: format!("moved to {to}"),
            });
            commit.files.insert(to, bytes);
        }
    }
}

fn rename_paths(
    commit: &mut Commit,
    from: &str,
    to: &str,
    label: &str,
    stripped: &mut Vec<StripEvent>,
) {
    let original = std::mem::take(&mut commit.files);
    let mut next = BTreeMap::new();
    for (path, bytes) in original {
        if let Some(renamed) = remap_path(&path, from, to) {
            if let Some(mode) = commit.modes.remove(&path) {
                commit.modes.insert(renamed.clone(), mode);
            }
            stripped.push(StripEvent {
                transform: label.to_string(),
                commit: commit.id.clone(),
                path: path.clone(),
                detail: format!("renamed to {renamed}"),
            });
            next.insert(renamed, bytes);
        } else {
            next.insert(path, bytes);
        }
    }
    commit.files = next;
}

fn remap_path(path: &str, from: &str, to: &str) -> Option<String> {
    if let Some(prefix) = from.strip_suffix("/**") {
        let prefix = prefix.trim_end_matches('/');
        let to = to.trim_end_matches('/');
        if path == prefix {
            return Some(to.to_string());
        }
        if let Some(rest) = path.strip_prefix(prefix).and_then(|s| s.strip_prefix('/')) {
            return Some(format!("{to}/{rest}"));
        }
        return None;
    }
    if path == from {
        return Some(to.to_string());
    }
    None
}

fn refuse_leftovers(
    commit: &Commit,
    path: &str,
    leftovers: &[&'static str],
    label: &str,
    refused: &mut Vec<StripEvent>,
) {
    for leftover in leftovers {
        refused.push(StripEvent {
            transform: label.to_string(),
            commit: commit.id.clone(),
            path: path.to_string(),
            detail: format!("leftover `{leftover}` after a stripped reference"),
        });
    }
}

fn rewrite_message(
    message: &str,
    budget_tokens: u32,
    hook_set: bool,
    rewriter: &dyn MessageRewriter,
    reference_regexes: &[Regex],
) -> String {
    if hook_set {
        if let Some(candidate) = rewriter.rewrite(message, budget_tokens) {
            let trimmed = candidate.trim();
            if !trimmed.is_empty() && estimate_tokens(trimmed) <= budget_tokens {
                return candidate;
            }
        }
    }
    fallback_message(message, reference_regexes)
}

fn fallback_message(message: &str, reference_regexes: &[Regex]) -> String {
    let first = first_line(message).to_string();
    let stripped = strip_references(&first, reference_regexes).text;
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim()
}

fn estimate_tokens(text: &str) -> u32 {
    text.split_whitespace().count() as u32
}

fn squash(commits: Vec<Commit>, authoring: &Authoring, reference_regexes: &[Regex]) -> Vec<Commit> {
    let Some(last) = commits.last() else {
        return Vec::new();
    };
    let subjects: Vec<String> = commits
        .iter()
        .map(|c| first_line(&c.message).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let joined = subjects.join("\n\n");
    let message = strip_references(&joined, reference_regexes).text;
    let message = rewrite_trailers(&message, authoring);
    vec![Commit {
        id: last.id.clone(),
        message,
        author_name: authoring.default.name.clone(),
        author_email: authoring.default.email.clone(),
        author_date: last.author_date.clone(),
        files: last.files.clone(),
        modes: last.modes.clone(),
    }]
}

fn map_author(authoring: &Authoring, name: &str, email: &str) -> (String, String) {
    for entry in &authoring.map {
        if entry.from.name == name && entry.from.email == email {
            return (entry.to.name.clone(), entry.to.email.clone());
        }
    }
    (
        authoring.default.name.clone(),
        authoring.default.email.clone(),
    )
}

fn map_trailer_identity<'a>(authoring: &'a Authoring, email: &str) -> Option<&'a Identity> {
    authoring
        .map
        .iter()
        .find(|entry| entry.from.email.eq_ignore_ascii_case(email))
        .map(|entry| &entry.to)
}

fn rewrite_trailers(message: &str, authoring: &Authoring) -> String {
    let (body, trailers) = split_trailers(message);
    if trailers.is_empty() {
        return message.trim_end().to_string();
    }
    let mut kept = Vec::new();
    for line in trailers {
        match trailer_parts(&line) {
            Some((key, rest))
                if key.eq_ignore_ascii_case("Signed-off-by")
                    || key.eq_ignore_ascii_case("Co-authored-by") =>
            {
                let Some(email) = trailer_email(rest) else {
                    continue;
                };
                let Some(to) = map_trailer_identity(authoring, email) else {
                    continue;
                };
                kept.push(format!("{key}: {} <{}>", to.name, to.email));
            }
            _ => kept.push(line),
        }
    }
    if kept.is_empty() {
        return body;
    }
    if body.is_empty() {
        kept.join("\n")
    } else {
        format!(
            "{}

{}",
            body,
            kept.join("\n")
        )
    }
}

fn split_trailers(message: &str) -> (String, Vec<String>) {
    let trimmed = message.trim_end();
    let mut lines: Vec<&str> = trimmed.lines().collect();
    let mut trailers = Vec::new();
    while let Some(line) = lines.last() {
        if is_trailer_line(line) {
            trailers.push(lines.pop().unwrap().to_string());
        } else {
            break;
        }
    }
    trailers.reverse();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    (lines.join("\n"), trailers)
}

fn is_trailer_line(line: &str) -> bool {
    trailer_parts(line).is_some()
}

fn trailer_parts(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some((key, rest))
}

fn trailer_email(rest: &str) -> Option<&str> {
    let start = rest.find('<')?;
    let end = rest[start + 1..].find('>')?;
    let email = rest[start + 1..start + 1 + end].trim();
    if email.is_empty() {
        None
    } else {
        Some(email)
    }
}

pub struct StubRewriter;

impl MessageRewriter for StubRewriter {
    fn rewrite(&self, _message: &str, _budget_tokens: u32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::parse_workflow;

    fn workflow(extra: &str) -> Workflow {
        parse_workflow(&format!(
            r#"
[workflow]
name = "t"
mode = "iterative"
[origin]
path = "origin"
ref = "main"
[destination]
path = "dest"
fetch = "main"
push = "main"
[paths.origin]
include = ["**"]
exclude = ["secret/**"]
[paths.destination]
include = ["**"]
[authoring]
default = {{ name = "Authors", email = "a@example.invalid" }}
{extra}
"#
        ))
        .unwrap()
    }

    fn commit(id: &str, message: &str, files: &[(&str, &str)]) -> Commit {
        Commit {
            id: id.to_string(),
            message: message.to_string(),
            author_name: "Ada".to_string(),
            author_email: "ada@internal.example".to_string(),
            author_date: None,
            files: files
                .iter()
                .map(|(p, b)| (p.to_string(), Blob::from(b.as_bytes())))
                .collect(),
            ..Default::default()
        }
    }

    fn held_bytes(commits: &[Commit]) -> usize {
        held_blobs(commits).0
    }

    // A copy of every file per commit of a window is several gigabytes for a large tree. The window's commits share the loader's copy of a file they carry, and an edit that gives each
    // commit the same bytes leaves them one copy between them.
    #[test]
    fn a_window_holds_one_copy_of_a_file_its_commits_carry_and_edit_alike() {
        let wf = workflow(
            r#"
[[transformations]]
type = "strip_references"
patterns = ["PROJ-[0-9a-f]{6}"]

[[transformations]]
type = "drop_empty_commits"
"#,
        );
        let big = Blob::from("a line with PROJ-abc123 in it\n".repeat(2048).as_bytes());
        let kept = Blob::from("a line with no reference in it\n".repeat(2048).as_bytes());
        let commits: Vec<Commit> = (0..32)
            .map(|n| {
                let mut commit = commit(
                    &format!("c{n:02}"),
                    "change",
                    &[("n.txt", &format!("{n}\n"))],
                );
                commit.files.insert("big.txt".to_string(), Arc::clone(&big));
                commit
                    .files
                    .insert("kept.txt".to_string(), Arc::clone(&kept));
                commit
            })
            .collect();
        let result = apply_window(
            &wf,
            commits,
            BTreeMap::new(),
            &StubRewriter,
            &mut WindowState::default(),
            false,
        );
        assert_eq!(result.commits.len(), 32);
        let edited = result.commits[0].files["big.txt"].len();
        assert!(edited < big.len(), "the reference was stripped");
        assert!(result
            .commits
            .iter()
            .all(|commit| Arc::ptr_eq(&commit.files["kept.txt"], &kept)));
        let small: usize = result.commits.iter().map(|c| c.files["n.txt"].len()).sum();
        assert_eq!(
            held_bytes(&result.commits),
            kept.len() + edited + small,
            "32 commits hold one copy of kept.txt and one of the edited big.txt"
        );
    }

    // An editing transformation reuses what it did to the commit before for the same blob at the same path.
    // Over a history where files change, stay, change back and move, every commit's tree and events (stripped and
    // refused) are what editing that commit on its own gives.
    #[test]
    fn an_edit_carried_from_the_commit_before_is_what_editing_the_commit_alone_gives() {
        let wf = workflow(
            r#"
[[transformations]]
type = "strip_references"
patterns = ["PROJ-[0-9a-f]{6}"]

[[transformations]]
type = "replace_text"
pattern = "secret"
replacement = "public"
on = "both"

[[transformations]]
type = "strip_comments"
languages = ["rust"]
"#,
        );
        let a = "PROJ-abc123 in a, secret too, (PROJ-abc123\n\n) refused\n";
        let b = "fn b() {} // PROJ-abc123 a comment\n";
        let history = [
            (
                "c1",
                "PROJ-abc123 first",
                vec![("a.txt", a), ("b.rs", b), ("c.txt", "plain\n")],
            ),
            (
                "c2",
                "second",
                vec![("a.txt", a), ("b.rs", b), ("c.txt", "secret plain\n")],
            ),
            (
                "c3",
                "third PROJ-abc123",
                vec![("a.txt", "changed ()\n"), ("b.rs", b)],
            ),
            (
                "c4",
                "fourth",
                vec![("a.txt", a), ("b.rs", b), ("d.txt", a)],
            ),
            (
                "c5",
                "fifth",
                vec![("a.txt", a), ("b2.rs", b), ("d.txt", a)],
            ),
        ];
        // One blob per distinct text, as the loader gives: the same bytes at the same path are the same blob.
        let mut blobs: BTreeMap<&str, Blob> = BTreeMap::new();
        let commits: Vec<Commit> = history
            .iter()
            .map(|(id, message, files)| {
                let mut c = commit(id, message, &[]);
                for (path, text) in files {
                    let blob = blobs
                        .entry(text)
                        .or_insert_with(|| Blob::from(text.as_bytes()));
                    c.files.insert(path.to_string(), Arc::clone(blob));
                }
                c
            })
            .collect();
        let walked = apply_window(
            &wf,
            commits.clone(),
            BTreeMap::new(),
            &StubRewriter,
            &mut WindowState::default(),
            false,
        );
        assert_eq!(walked.commits.len(), commits.len());
        assert!(
            !walked.refused.is_empty(),
            "the history has a leftover to refuse"
        );
        for (index, commit) in commits.into_iter().enumerate() {
            let id = commit.id.clone();
            let alone = apply_window(
                &wf,
                vec![commit],
                BTreeMap::new(),
                &StubRewriter,
                &mut WindowState::default(),
                false,
            );
            assert_eq!(walked.commits[index].files, alone.commits[0].files, "{id}");
            let of = |events: &[StripEvent]| -> Vec<StripEvent> {
                events.iter().filter(|e| e.commit == id).cloned().collect()
            };
            assert_eq!(of(&walked.stripped), alone.stripped, "{id}");
            assert_eq!(of(&walked.refused), alone.refused, "{id}");
        }
    }

    // A range's subjects are read as the squash carries them: the message transformations applied, the files
    // aside, so a term a replace_text rewrites in messages is not a dirty subject.
    #[test]
    fn squash_subjects_are_the_messages_as_the_transformations_leave_them() {
        let wf = workflow(
            r#"
[[transformations]]
type = "strip_references"
patterns = ["PROJ-[0-9a-f]{6}"]

[[transformations]]
type = "replace_text"
pattern = "hostname-zebra[0-9]"
replacement = "a build host"
on = "messages"

[[transformations]]
type = "replace_text"
pattern = "files only"
replacement = "rewritten"
on = "files"
"#,
        );
        let commits = [
            commit(
                "c1",
                "PROJ-abc123 moved to hostname-zebra7\n\nbody",
                &[("a.txt", "x\n")],
            ),
            commit("c2", "files only stay", &[]),
        ];
        assert_eq!(
            squash_subjects(&wf, &commits),
            vec![
                "moved to a build host".to_string(),
                "files only stay".to_string()
            ]
        );
    }

    // A file that is the blob (or the bytes) it read at
    // that path in the commit before keeps its verdict without being searched again, across a window boundary too,
    // and every commit loses what stripping it on its own loses.
    #[test]
    fn strip_matching_searches_a_file_once_while_its_bytes_stay() {
        let wf = workflow(
            r#"
[[transformations]]
type = "strip_matching"
paths = ["**"]
patterns = ["do not ship"]
"#,
        );
        let flagged = Blob::from("line one\ndo not ship\n".repeat(64).as_bytes());
        let clean = Blob::from("a clean line\n".repeat(64).as_bytes());
        let window = |ids: std::ops::Range<usize>, flagged: &Blob, clean: &Blob| -> Vec<Commit> {
            ids.map(|n| {
                let mut c = commit(
                    &format!("c{n:02}"),
                    "change",
                    &[("n.txt", &format!("{n}\n"))],
                );
                c.files
                    .insert("flagged.txt".to_string(), Arc::clone(flagged));
                c.files.insert("clean.txt".to_string(), Arc::clone(clean));
                c
            })
            .collect()
        };
        let mut state = WindowState::default();
        STRIP_SEARCHES.with(|count| count.set(0));
        let first = apply_window(
            &wf,
            window(0..8, &flagged, &clean),
            BTreeMap::new(),
            &StubRewriter,
            &mut state,
            false,
        );
        // The next window's loader reads the same files again: new blobs with the same bytes.
        let (flagged_again, clean_again) = (Blob::from(&flagged[..]), Blob::from(&clean[..]));
        let second = apply_window(
            &wf,
            window(8..16, &flagged_again, &clean_again),
            BTreeMap::new(),
            &StubRewriter,
            &mut state,
            false,
        );
        let searches = STRIP_SEARCHES.with(std::cell::Cell::get);
        // n.txt changes every commit (16 searches); flagged.txt and clean.txt are searched once each.
        assert_eq!(searches, 16 + 2, "an unchanged file is searched once");
        for result in [&first, &second] {
            for c in &result.commits {
                assert!(!c.files.contains_key("flagged.txt"), "{}", c.id);
                assert!(c.files.contains_key("clean.txt"), "{}", c.id);
                let alone = apply_window(
                    &wf,
                    vec![commit(
                        &c.id,
                        "change",
                        &[
                            ("n.txt", std::str::from_utf8(&c.files["n.txt"]).unwrap()),
                            ("flagged.txt", std::str::from_utf8(&flagged).unwrap()),
                            ("clean.txt", std::str::from_utf8(&clean).unwrap()),
                        ],
                    )],
                    BTreeMap::new(),
                    &StubRewriter,
                    &mut WindowState::default(),
                    false,
                );
                let of = |events: &[StripEvent]| -> Vec<StripEvent> {
                    events
                        .iter()
                        .filter(|e| e.commit == c.id)
                        .cloned()
                        .collect()
                };
                assert_eq!(of(&result.stripped), alone.stripped, "{}", c.id);
                assert_eq!(c.files, alone.commits[0].files, "{}", c.id);
            }
        }
    }

    // The loader reads each window's blobs afresh, so a memo keyed by blob alone would edit the whole tree again at
    // the first commit of every window. The same bytes at the same path take the window before's edited copy.
    #[test]
    fn an_edit_is_carried_across_a_window_boundary_for_the_same_bytes() {
        let wf = workflow(
            r#"
[[transformations]]
type = "replace_text"
pattern = "secret"
replacement = "public"
on = "files"
"#,
        );
        let text = "a secret line\n".repeat(64);
        let mut state = WindowState::default();
        let first = apply_window(
            &wf,
            vec![commit("c1", "one", &[("a.txt", &text)])],
            BTreeMap::new(),
            &StubRewriter,
            &mut state,
            false,
        );
        let second = apply_window(
            &wf,
            vec![commit("c2", "two", &[("a.txt", &text)])],
            BTreeMap::new(),
            &StubRewriter,
            &mut state,
            false,
        );
        assert!(
            Arc::ptr_eq(
                &first.commits[0].files["a.txt"],
                &second.commits[0].files["a.txt"]
            ),
            "the second window holds the first window's edited copy"
        );
        let alone = apply_window(
            &wf,
            vec![commit("c2", "two", &[("a.txt", &text)])],
            BTreeMap::new(),
            &StubRewriter,
            &mut WindowState::default(),
            false,
        );
        assert_eq!(second.commits[0].files, alone.commits[0].files);
        assert_eq!(second.stripped, alone.stripped);
    }

    #[test]
    fn origin_exclude_and_strip_paths_and_fallback_rewrite() {
        let wf = workflow(
            r#"
[[transformations]]
type = "strip_paths"
globs = [".logbook/**"]

[[transformations]]
type = "strip_references"
patterns = ["PROJ-[0-9a-f]{6}", "logbook"]

[[transformations]]
type = "rewrite_messages"
hook = "low_cost_model"
fallback = "first_line_strip_references"

[[transformations]]
type = "drop_empty_commits"
"#,
        );
        let commits = vec![
            commit(
                "aaa",
                "PROJ-abc123 add tracker\n\nlogbook details",
                &[
                    ("keep.txt", "hello logbook"),
                    (".logbook/item.md", "secret"),
                    ("secret/x.txt", "nope"),
                ],
            ),
            commit("bbb", "empty after strip", &[(".logbook/item.md", "gone")]),
        ];
        let result = apply(&wf, commits, BTreeMap::new(), &StubRewriter);
        assert_eq!(result.commits.len(), 1);
        assert_eq!(result.commits[0].id, "aaa");
        assert_eq!(result.rewritten[0].rewritten_subject, "add tracker");
        assert_eq!(
            std::str::from_utf8(&result.commits[0].files["keep.txt"]).unwrap(),
            "hello "
        );
        assert!(!result.commits[0].files.contains_key(".logbook/item.md"));
        assert!(!result.commits[0].files.contains_key("secret/x.txt"));
        assert!(result.drops.iter().any(|d| d.commit == "bbb"));
    }

    // The parent tree a resumed walk starts from is the RAW tree at resolved_since; the first walked
    // commit is compared with that tree as the transformations before drop_empty_commits leave it.
    #[test]
    fn the_first_commit_is_compared_with_the_transformed_parent() {
        let wf = workflow(
            r#"
[[transformations]]
type = "strip_paths"
globs = [".logbook/**"]

[[transformations]]
type = "strip_references"
patterns = ["PROJ-[0-9a-f]{6}", "logbook"]

[[transformations]]
type = "drop_empty_commits"
"#,
        );
        let parent: BTreeMap<String, Blob> = [
            ("keep.txt", "hello logbook"),
            (".logbook/item.md", "a"),
            ("secret/x.txt", "nope"),
        ]
        .iter()
        .map(|(p, b)| (p.to_string(), Blob::from(b.as_bytes())))
        .collect();
        let commits = vec![
            commit(
                "ccc",
                "tracker only",
                &[("keep.txt", "hello logbook"), (".logbook/item.md", "b")],
            ),
            commit(
                "ddd",
                "real change",
                &[("keep.txt", "bye logbook"), (".logbook/item.md", "b")],
            ),
        ];
        let result = apply(&wf, commits, parent, &StubRewriter);
        assert_eq!(
            result.drops,
            vec![Drop {
                commit: "ccc".to_string(),
                reason: "empty after strip".to_string(),
            }]
        );
        assert_eq!(result.commits.len(), 1);
        assert_eq!(result.commits[0].id, "ddd");
        // The parent's own transformation leaves no trace: every event names a walked commit.
        assert!(result
            .stripped
            .iter()
            .all(|e| e.commit == "ccc" || e.commit == "ddd"));
        assert!(!result.files_stripped.contains("secret/x.txt"));
    }

    #[test]
    fn squash_joins_rewritten_first_lines() {
        let mut wf = workflow(
            r#"
[[transformations]]
type = "rewrite_messages"
hook = "low_cost_model"
"#,
        );
        wf.mode = Mode::Squash;
        let commits = vec![
            commit("aaa", "one\n\nbody", &[("a.txt", "1")]),
            commit("bbb", "two\n\nbody", &[("a.txt", "2")]),
        ];
        let result = apply(&wf, commits, BTreeMap::new(), &StubRewriter);
        assert_eq!(result.commits.len(), 1);
        assert_eq!(result.commits[0].message, "one\n\ntwo");
        assert_eq!(result.commits[0].author_name, "Authors");
        assert_eq!(
            std::str::from_utf8(&result.commits[0].files["a.txt"]).unwrap(),
            "2"
        );
    }

    #[test]
    fn iterative_maps_author() {
        let wf = workflow(
            r#"
[[authoring.map]]
from = { name = "Ada", email = "ada@internal.example" }
to = { name = "Ada", email = "ada@example.invalid" }
"#,
        );
        let commits = vec![commit("aaa", "one", &[("a.txt", "1")])];
        let result = apply(&wf, commits, BTreeMap::new(), &StubRewriter);
        assert_eq!(result.commits[0].author_name, "Ada");
        assert_eq!(result.commits[0].author_email, "ada@example.invalid");
    }

    #[test]
    fn unmapped_authors_become_default() {
        let wf = workflow("");
        let commits = vec![commit("aaa", "one", &[("a.txt", "1")])];
        let result = apply(&wf, commits, BTreeMap::new(), &StubRewriter);
        assert_eq!(result.commits[0].author_name, "Authors");
        assert_eq!(result.commits[0].author_email, "a@example.invalid");
    }

    #[test]
    fn trailer_emails_map_or_drop() {
        let wf = workflow(
            r#"
[[authoring.map]]
from = { name = "Ada", email = "ada@internal.example" }
to = { name = "Ada", email = "ada@example.invalid" }
"#,
        );
        let commits = vec![commit(
            "aaa",
            "one\n\nSigned-off-by: Ada <ada@internal.example>\nCo-authored-by: Bob <bob@internal.example>\n",
            &[("a.txt", "1")],
        )];
        let result = apply(&wf, commits, BTreeMap::new(), &StubRewriter);
        let msg = &result.commits[0].message;
        assert!(
            msg.contains("Signed-off-by: Ada <ada@example.invalid>"),
            "{msg}"
        );
        assert!(!msg.contains("bob@internal.example"), "{msg}");
        assert!(!msg.contains("Co-authored-by"), "{msg}");
    }

    #[test]
    fn rename_paths_preserves_layout() {
        let wf = workflow(
            r#"
[[transformations]]
type = "rename_paths"
from = "crates/private-name/**"
to = "crates/public-name/"
"#,
        );
        let commits = vec![commit(
            "aaa",
            "move",
            &[("crates/private-name/src/lib.rs", "x")],
        )];
        let result = apply(&wf, commits, BTreeMap::new(), &StubRewriter);
        assert!(result.commits[0]
            .files
            .contains_key("crates/public-name/src/lib.rs"));
    }
}

/// A stripped reference takes its punctuation with it.
#[cfg(test)]
mod strip_tests {
    use super::*;
    use crate::workflow::parse_workflow;

    fn strip(text: &str) -> (String, Vec<String>) {
        let wf = parse_workflow(
            r#"
[workflow]
name = "t"
mode = "iterative"
[origin]
path = "origin"
ref = "main"
[destination]
path = "dest"
fetch = "main"
push = "main"
[paths.origin]
include = ["**"]
[paths.destination]
include = ["**"]
[authoring]
default = { name = "Authors", email = "a@example.invalid" }
[[transformations]]
type = "strip_references"
patterns = ["PROJ-[0-9a-f]{6}", "logbook"]
"#,
        )
        .unwrap();
        let commit = Commit {
            id: "aaa".to_string(),
            message: "m".to_string(),
            author_name: "Ada".to_string(),
            author_email: "ada@example.invalid".to_string(),
            author_date: None,
            files: [("f.md".to_string(), Blob::from(text.as_bytes()))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let result = apply(&wf, vec![commit], BTreeMap::new(), &StubRewriter);
        let refused = result
            .refused
            .iter()
            .map(|e| e.detail.clone())
            .collect::<Vec<_>>();
        let text = result
            .commits
            .first()
            .map(|c| String::from_utf8(c.files["f.md"].to_vec()).unwrap())
            .unwrap_or_default();
        (text, refused)
    }

    #[test]
    fn a_reference_takes_its_list_item_and_parenthetical() {
        for (input, want) in [
            ("(PROJ-c0ffee, seen on v2.1)", "(seen on v2.1)"),
            ("PROJ-facade; it replaced", "it replaced"),
            ("round 1 (PROJ-decade): a", "round 1: a"),
            ("round 2 (beta, PROJ-bead01) adds", "round 2 (beta) adds"),
            ("(a, PROJ-abc123, b)", "(a, b)"),
            ("(PROJ-abc123, PROJ-def456) x", "x"),
            ("x (PROJ-abc123; PROJ-def456).", "x."),
            ("since PROJ-bead02, the id", "since, the id"),
            ("landed in v2.2, PROJ-abc123.", "landed in v2.2."),
            ("a; PROJ-abc123; b", "a; b"),
            ("hello logbook", "hello "),
            ("fn keep() {} // PROJ-abc123\n", "fn keep() {} // \n"),
        ] {
            let (got, refused) = strip(input);
            assert_eq!(got, want, "input {input:?}");
            assert!(refused.is_empty(), "input {input:?}: {refused:?}");
        }
    }

    #[test]
    fn a_docs_header_owner_field_goes_whole() {
        for (input, want) in [
            (
                "<!-- docs: verified=2026-10-11; owner=PROJ-f00d01 -->",
                "<!-- docs: verified=2026-10-11 -->",
            ),
            (
                "<!-- docs: owner=PROJ-f00d01; verified=2026-10-11 -->",
                "<!-- docs: verified=2026-10-11 -->",
            ),
        ] {
            let (got, refused) = strip(input);
            assert_eq!(got, want, "input {input:?}");
            assert!(refused.is_empty(), "{refused:?}");
        }
    }

    #[test]
    fn the_second_pass_refuses_leftover_punctuation() {
        // A blank line inside the parenthetical (a single line break goes whole).
        let (_, refused) = strip("a parenthetical (PROJ-abc123\n\n) across a paragraph");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("()"), "{refused:?}");
    }
}
