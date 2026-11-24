use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use copybera_rules::rust_cascade::{cascade_rust_in, CascadeSummary};
use copybera_rules::{
    apply, apply_window, finish_squash, first_line, held_blobs, parse_workflow, Blob, Commit,
    LintAllow, Mode, OriginRange, PathFilter, Receipt, ReceiptLintAllow, ReceiptVerify, StripEvent,
    StubRewriter, TransformResult, Transformation, Verify, WindowState, Workflow,
};
use regex::Regex;

use crate::git::{Author, Git, TreeRef};

pub fn validate(path: &Path) -> Result<Workflow> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("{}: cannot read workflow", path.display()))?;
    parse_workflow(&text).map_err(Into::into)
}

/// How many commits' trees a squash walk holds at once. A squash keeps the last tree and the subjects, so the walk
/// runs in windows (a walk that held every commit's whole tree, a clone per commit and path, grows with the history
/// until the machine runs out of memory). `COPYBERA_TREE_WINDOW` overrides it (the bound test uses a small one).
const DEFAULT_TREE_WINDOW: usize = 32;

fn tree_window() -> usize {
    let Ok(value) = std::env::var("COPYBERA_TREE_WINDOW") else {
        return DEFAULT_TREE_WINDOW;
    };
    match value.trim().parse::<usize>() {
        Ok(window) if window > 0 => window,
        _ => {
            eprintln!(
                "copybera: COPYBERA_TREE_WINDOW={value:?} is not a positive whole number; using {DEFAULT_TREE_WINDOW}"
            );
            DEFAULT_TREE_WINDOW
        }
    }
}

/// What a squash walk discards window by window, counted: the strip events of commits that are no longer the last
/// kept one, by transformation, and the distinct paths moved and left in place.
#[derive(Debug, Default)]
struct FoldedEvents {
    by_transform: BTreeMap<String, u64>,
    moved: BTreeSet<String>,
    stayed: BTreeSet<String>,
}

impl FoldedEvents {
    fn fold(&mut self, event: &StripEvent) {
        *self
            .by_transform
            .entry(event.transform.clone())
            .or_default() += 1;
        if event.detail.starts_with("moved to ") || event.detail.starts_with("renamed to ") {
            self.moved.insert(event.path.clone());
        } else if event.detail.starts_with("stays: target taken") {
            self.stayed.insert(event.path.clone());
        }
    }

    fn total(&self) -> u64 {
        self.by_transform.values().sum()
    }
}

type MovedNames = BTreeMap<usize, BTreeMap<String, String>>;

/// A squash walk applied in windows of `window` commits: each window's trees are loaded, transformed and shrunk to
/// the last kept commit's tree and the commits' subjects before the next window loads, then the windows are squashed
/// together. A workflow whose `follow_moves` reads the names moves record walks first to find them, so a message in
/// an early window is rewritten as a one-pass walk rewrites it (the walk repeats until the names settle).
fn walk_squash(
    git: &Git,
    origin: &Path,
    workflow: &Workflow,
    ids: &[Commit],
    since_sha: Option<&str>,
    window: usize,
) -> Result<(TransformResult, FoldedEvents)> {
    let follows = workflow.transformations.iter().any(|transform| {
        matches!(
            transform,
            Transformation::ReplacePairs {
                follow_moves: true,
                ..
            }
        )
    });
    if !follows {
        let (result, folded, _) = squash_pass(git, origin, workflow, ids, since_sha, window, None)?;
        return Ok((result, folded));
    }
    let (_, _, mut seed) = squash_pass(git, origin, workflow, ids, since_sha, window, None)?;
    for _ in 0..4 {
        let (result, folded, recorded) = squash_pass(
            git,
            origin,
            workflow,
            ids,
            since_sha,
            window,
            Some(seed.clone()),
        )?;
        if recorded == seed {
            return Ok((result, folded));
        }
        seed = recorded;
    }
    Err(anyhow!(
        "the names the move_paths transformations record did not settle in 5 walks"
    ))
}

fn squash_pass(
    git: &Git,
    origin: &Path,
    workflow: &Workflow,
    ids: &[Commit],
    since_sha: Option<&str>,
    window: usize,
    seed: Option<MovedNames>,
) -> Result<(TransformResult, FoldedEvents, MovedNames)> {
    let mut state = WindowState {
        seed,
        ..WindowState::default()
    };
    let mut folded = FoldedEvents::default();
    let mut total: Option<TransformResult> = None;
    let (mut blobs_read, mut files_read, mut index) = (0usize, 0usize, 0usize);
    let windows = ids.len().div_ceil(window);
    for (window_index, chunk) in ids.chunks(window).enumerate() {
        let load_parent = window_index == 0 && since_sha.is_some();
        let mut treeishes: Vec<String> = Vec::with_capacity(chunk.len() + 1);
        if let (true, Some(sha)) = (load_parent, since_sha) {
            treeishes.push(sha.to_string());
        }
        treeishes.extend(chunk.iter().map(|header| header.id.clone()));
        let loaded_trees =
            git.load_trees_filtered_once(origin, &treeishes, &workflow.origin_paths)?;
        blobs_read += loaded_trees.blobs_read;
        files_read += loaded_trees.files_read;
        let mut trees = loaded_trees.trees.into_iter();
        let mut tree_modes = loaded_trees.modes.into_iter();
        // Only the first window needs the walk's parent tree: the drop baselines carry from there.
        let parent = if load_parent {
            tree_modes.next();
            trees
                .next()
                .expect("the parent tree was included in the requested trees")
        } else {
            BTreeMap::new()
        };
        let mut loaded = Vec::with_capacity(chunk.len());
        for header in chunk {
            index += 1;
            eprintln!(
                "loading origin commit {}/{}: {}",
                index,
                ids.len(),
                short(&header.id)
            );
            let mut commit = header.clone();
            commit.files = trees.next().expect("each requested commit tree was loaded");
            commit.modes = tree_modes.next().unwrap_or_default();
            loaded.push(commit);
        }
        let (loaded_bytes, loaded_blobs) = held_blobs(&loaded);
        let part = apply_window(workflow, loaded, parent, &StubRewriter, &mut state, false);
        let (held_bytes, held_count) = held_blobs(&part.commits);
        // What the window's trees hold, shared blobs counted once: a tree's bytes plus what the commits change,
        // not the window's length times the tree.
        eprintln!(
            "window {} bytes held: {loaded_bytes} in {loaded_blobs} blobs as loaded, {held_bytes} in {held_count} blobs transformed",
            window_index + 1
        );
        total = Some(match total.take() {
            None => part,
            Some(mut sum) => {
                sum.commits.extend(part.commits);
                sum.stripped.extend(part.stripped);
                sum.drops.extend(part.drops);
                sum.rewritten.extend(part.rewritten);
                sum.files_stripped.extend(part.files_stripped);
                sum.refused.extend(part.refused);
                sum
            }
        });
        if let Some(sum) = total.as_mut() {
            // The squash takes the last kept commit's tree and every commit's subject: keep nothing else.
            let last = sum.commits.len().saturating_sub(1);
            for (position, commit) in sum.commits.iter_mut().enumerate() {
                if position < last {
                    commit.files = BTreeMap::new();
                    commit.modes = BTreeMap::new();
                }
                commit.message = first_line(&commit.message).to_string();
            }
            // Events of commits that can no longer be the squash's are counted, not kept, until the last window.
            if window_index + 1 < windows {
                let keep = sum.commits.last().map(|commit| commit.id.clone());
                let events = std::mem::take(&mut sum.stripped);
                for event in events {
                    if Some(&event.commit) == keep.as_ref() {
                        sum.stripped.push(event);
                    } else {
                        folded.fold(&event);
                    }
                }
            }
        }
        eprintln!(
            "tree window {} held {} commits",
            window_index + 1,
            chunk.len()
        );
    }
    if windows > 1 {
        eprintln!(
            "blobs read {blobs_read} for {files_read} files (a blob shared across windows counts once per window)"
        );
    } else {
        eprintln!("blobs read {blobs_read} for {files_read} files");
    }
    let Some(mut result) = total else {
        // An empty walk: `apply` has already squashed nothing.
        return Ok((
            apply(workflow, Vec::new(), BTreeMap::new(), &StubRewriter),
            folded,
            state.moved,
        ));
    };
    result.commits = finish_squash(workflow, std::mem::take(&mut result.commits));
    Ok((result, folded, state.moved))
}

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Parses a positive duration written as seconds, minutes, or hours (for example, `10s`, `5m`, or `1h`).
pub fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let (number, unit) = value
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map(|(index, _)| value.split_at(index))
        .ok_or_else(|| "duration must end in s, m, or h".to_string())?;
    if number.is_empty() || unit.len() != 1 {
        return Err("duration must be a positive number followed by s, m, or h".to_string());
    }
    let count: u64 = number
        .parse()
        .map_err(|_| "duration must be a positive number followed by s, m, or h".to_string())?;
    if count == 0 {
        return Err("duration must be greater than zero".to_string());
    }
    let seconds_per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        _ => return Err("duration unit must be s, m, or h".to_string()),
    };
    count
        .checked_mul(seconds_per_unit)
        .map(Duration::from_secs)
        .ok_or_else(|| "duration is too large".to_string())
}

/// Parses a positive number of status tables to print.
pub fn parse_count(value: &str) -> std::result::Result<usize, String> {
    let count: usize = value
        .parse()
        .map_err(|_| "count must be a positive whole number".to_string())?;
    if count == 0 {
        return Err("count must be greater than zero".to_string());
    }
    Ok(count)
}

/// Parses a scheduled migration interval, which may not be shorter than one minute.
pub fn parse_schedule_duration(value: &str) -> std::result::Result<Duration, String> {
    let duration = parse_duration(value)?;
    if duration < Duration::from_secs(60) {
        return Err("scheduled duration must be at least 60 seconds".to_string());
    }
    Ok(duration)
}

/// Runs, plans, or repeats a migration on a schedule.
#[allow(clippy::too_many_arguments)]
pub fn scheduled_migrate(
    path: &Path,
    every: Duration,
    plan: bool,
    once: bool,
    force_dry_run: bool,
    max_commits: Option<usize>,
    resume: bool,
    git: &Git,
    out: &mut dyn Write,
) -> Result<()> {
    if plan {
        let command = scheduled_command(path, force_dry_run, max_commits, resume);
        let now = SystemTime::now();
        for offset in 0..3 {
            let delay = every
                .checked_mul(offset)
                .ok_or_else(|| anyhow!("scheduled duration is too large"))?;
            let when = now
                .checked_add(delay)
                .ok_or_else(|| anyhow!("scheduled time is too large"))?;
            writeln!(out, "{}: {command}", utc_timestamp(when)?)?;
        }
        return Ok(());
    }

    if once {
        migrate_scheduled(path, force_dry_run, max_commits, resume, git, out)?;
        return Ok(());
    }

    let _signals = SignalGuard::install()?;
    loop {
        if STOP_REQUESTED.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Err(error) = migrate_scheduled(path, force_dry_run, max_commits, resume, git, out) {
            eprintln!(
                "{}: scheduled migrate failed: {error:#}",
                utc_timestamp(SystemTime::now())?
            );
        }
        if STOP_REQUESTED.load(Ordering::Relaxed) {
            return Ok(());
        }
        sleep_until_stop(every);
    }
}

fn scheduled_command(
    path: &Path,
    force_dry_run: bool,
    max_commits: Option<usize>,
    resume: bool,
) -> String {
    let mut command = format!("copybera migrate {}", path.display());
    if force_dry_run {
        command.push_str(" --dry-run");
    }
    if let Some(max_commits) = max_commits {
        command.push_str(&format!(" --max-commits {max_commits}"));
    }
    if resume {
        command.push_str(" --resume");
    }
    command
}

fn sleep_until_stop(duration: Duration) {
    let started = Instant::now();
    while !STOP_REQUESTED.load(Ordering::Relaxed) {
        let elapsed = started.elapsed();
        if elapsed >= duration {
            return;
        }
        std::thread::sleep((duration - elapsed).min(Duration::from_millis(100)));
    }
}

fn utc_timestamp(time: SystemTime) -> Result<String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_date(days);
    Ok(format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
    ))
}

fn civil_date(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

extern "C" fn request_stop(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

struct SignalGuard {
    sigint: libc::sighandler_t,
    sigterm: libc::sighandler_t,
}

impl SignalGuard {
    fn install() -> Result<Self> {
        STOP_REQUESTED.store(false, Ordering::Relaxed);
        // SAFETY: `request_stop` has the C signal-handler ABI and only stores to an atomic flag.
        unsafe {
            let sigint = libc::signal(libc::SIGINT, request_stop as libc::sighandler_t);
            if sigint == libc::SIG_ERR {
                return Err(anyhow!("cannot install SIGINT handler"));
            }
            let sigterm = libc::signal(libc::SIGTERM, request_stop as libc::sighandler_t);
            if sigterm == libc::SIG_ERR {
                libc::signal(libc::SIGINT, sigint);
                return Err(anyhow!("cannot install SIGTERM handler"));
            }
            Ok(Self { sigint, sigterm })
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        // SAFETY: these are the handlers returned by `signal` when this guard was installed.
        unsafe {
            libc::signal(libc::SIGINT, self.sigint);
            libc::signal(libc::SIGTERM, self.sigterm);
        }
    }
}

pub fn migrate(
    path: &Path,
    force_dry_run: bool,
    max_commits: Option<usize>,
    resume: bool,
    git: &Git,
    out: &mut dyn Write,
) -> Result<PathBuf> {
    migrate_inner(path, force_dry_run, max_commits, resume, false, git, out)
}

fn migrate_scheduled(
    path: &Path,
    force_dry_run: bool,
    max_commits: Option<usize>,
    resume: bool,
    git: &Git,
    out: &mut dyn Write,
) -> Result<PathBuf> {
    migrate_inner(path, force_dry_run, max_commits, resume, true, git, out)
}

fn migrate_inner(
    path: &Path,
    force_dry_run: bool,
    max_commits: Option<usize>,
    resume: bool,
    scheduled: bool,
    git: &Git,
    out: &mut dyn Write,
) -> Result<PathBuf> {
    let started = Instant::now();
    let mut workflow = validate(path)?;
    if workflow.needs_private_patterns() {
        let file = private_patterns_path(path, &workflow).ok_or_else(|| {
            anyhow!(
                "strip_matching: private = true needs [lint] private (the private-patterns file)"
            )
        })?;
        let patterns = PrivatePatterns::load(&file)?;
        workflow.set_private_patterns(&patterns.regexes);
    }
    // A stripping workflow reads its private patterns before anything is walked.
    let private_strip = if workflow.lint_private_strip {
        let file = private_patterns_path(path, &workflow).ok_or_else(|| {
            anyhow!(
                "lint.private_action = \"strip\" needs [lint] private (the private-patterns file)"
            )
        })?;
        Some(PrivatePatterns::load(&file)?)
    } else {
        None
    };
    let dry_run = force_dry_run || workflow.dry_run;

    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let origin_path = workflow.origin.location.path.as_ref().ok_or_else(|| {
        anyhow!("origin.path: migrate walks a local checkout; origin.url is not supported yet")
    })?;
    let origin = resolve(base, origin_path);
    if !origin.is_dir() {
        return Err(anyhow!(
            "origin.path: {} is not a directory",
            origin.display()
        ));
    }
    if !git.is_repo(&origin) {
        return Err(anyhow!(
            "origin.path: {} is not a git repository",
            origin.display()
        ));
    }

    let dest = if dry_run {
        workflow
            .destination
            .location
            .path
            .as_ref()
            .map(|dest_path| resolve(base, dest_path))
    } else {
        let dest_path = workflow.destination.location.path.as_ref().ok_or_else(|| {
            anyhow!("destination.path: migrate writes a local repository; destination.url is not supported yet")
        })?;
        let dest = resolve(base, dest_path);
        if !dest.is_dir() {
            return Err(anyhow!(
                "destination.path: {} is not a directory",
                dest.display()
            ));
        }
        if !git.is_repo(&dest) {
            return Err(anyhow!(
                "destination.path: {} is not a git repository",
                dest.display()
            ));
        }
        if git.is_dirty(&dest)? {
            return Err(anyhow!("destination.path: {} is dirty", dest.display()));
        }
        Some(dest)
    };

    let resolved_ref = git
        .rev_parse(&origin, &workflow.origin.ref_name)
        .with_context(|| format!("origin.ref: cannot resolve {}", workflow.origin.ref_name))?;

    let resume_receipt = if resume {
        let receipt_path = receipt_path(path, &workflow);
        if scheduled && !receipt_path.exists() {
            None
        } else {
            let receipt = read_receipt(&receipt_path)?;
            if receipt.dry_run {
                return Err(anyhow!("resume: receipt is from a dry-run"));
            }
            if !receipt.bounded {
                if scheduled {
                    None
                } else {
                    return Err(anyhow!("resume: receipt is not bounded"));
                }
            } else {
                let last_rev = receipt
                    .last_rev
                    .as_deref()
                    .ok_or_else(|| anyhow!("resume: receipt has no last_rev"))?;
                if !git.is_ancestor(&origin, last_rev, &resolved_ref)? {
                    return Err(anyhow!(
                        "resume: receipt last_rev {last_rev} is not an ancestor of origin ref {}",
                        workflow.origin.ref_name
                    ));
                }
                Some(receipt)
            }
        }
    } else {
        None
    };

    let mut resolved_since = None;
    // A scheduled `--resume` cycle with no bounded receipt walks on from the destination's
    // Copybera-RevId; a destination with none yet starts where a plain migrate does (origin.since).
    let walk_on = match &dest {
        Some(dest) if resume_receipt.is_none() && scheduled && resume => {
            git.copybera_rev_id(dest)?
        }
        _ => None,
    };
    let since_sha = if let Some(receipt) = &resume_receipt {
        resolved_since = receipt.last_rev.clone();
        receipt.last_rev.clone()
    } else if let Some(rev) = walk_on {
        resolved_since = Some(rev.clone());
        Some(rev)
    } else if let Some(since) = &workflow.origin.since {
        let sha = git
            .rev_parse(&origin, since)
            .with_context(|| format!("origin.since: cannot resolve {since}"))?;
        resolved_since = Some(sha.clone());
        Some(sha)
    } else if let Some(dest) = &dest {
        git.copybera_rev_id(dest)?
    } else {
        None
    };

    let mut ids = git.rev_list(&origin, since_sha.as_deref(), &resolved_ref)?;
    let commits_in_range = ids.len();
    if let Some(max_commits) = max_commits {
        ids.truncate(max_commits);
    }
    let commits_read = ids.len() as u64;
    // A bounded run claims only what it walked: its last walked commit is
    // the receipt's last_rev, the squash's origin id and the destination's Copybera-RevId, so a
    // resumed run starts after it rather than after the whole ref.
    let bounded = ids.len() < commits_in_range;
    let last_rev = if bounded {
        ids.last().cloned().or_else(|| since_sha.clone())
    } else {
        Some(resolved_ref.clone())
    };
    let head_rev = last_rev.clone().unwrap_or_else(|| resolved_ref.clone());
    // Every walked commit's header from one git stream, not one spawn per commit.
    let headers = git.load_commit_headers(&origin, &ids)?;
    if workflow.origin.subjects_clean {
        // Before anything is walked: a squash message carries every subject.
        crate::subjects::require_clean(
            path,
            &workflow,
            &headers,
            since_sha.as_deref(),
            &resolved_ref,
        )?;
    }
    let (mut result, folded) = if workflow.mode == Mode::Squash {
        walk_squash(
            git,
            &origin,
            &workflow,
            &headers,
            since_sha.as_deref(),
            tree_window(),
        )?
    } else {
        let mut treeishes = Vec::with_capacity(ids.len() + usize::from(since_sha.is_some()));
        if let Some(sha) = &since_sha {
            treeishes.push(sha.clone());
        }
        treeishes.extend(ids.iter().cloned());
        let loaded_trees =
            git.load_trees_filtered_once(&origin, &treeishes, &workflow.origin_paths)?;
        let blobs_read = loaded_trees.blobs_read;
        let files_read = loaded_trees.files_read;
        let mut trees = loaded_trees.trees.into_iter();
        let mut tree_modes = loaded_trees.modes.into_iter();
        let parent = if since_sha.is_some() {
            tree_modes.next();
            trees
                .next()
                .expect("the parent tree was included in the requested trees")
        } else {
            BTreeMap::new()
        };

        let mut loaded = Vec::with_capacity(ids.len());
        for (index, header) in headers.into_iter().enumerate() {
            eprintln!(
                "loading origin commit {}/{}: {}",
                index + 1,
                ids.len(),
                short(&header.id)
            );
            let mut commit = header;
            commit.files = trees.next().expect("each requested commit tree was loaded");
            commit.modes = tree_modes.next().unwrap_or_default();
            loaded.push(commit);
        }
        eprintln!("blobs read {blobs_read} for {files_read} files");

        (
            apply(&workflow, loaded, parent, &StubRewriter),
            FoldedEvents::default(),
        )
    };
    let private_scrub = scrub_rules_path(path, &workflow)
        .as_deref()
        .map(ScrubRules::load)
        .transpose()?;
    if let Some(scrub) = &private_scrub {
        scrub.apply(&mut result.commits, &mut result.stripped);
        for rewritten in &mut result.rewritten {
            rewritten.original_subject = scrub.redact(&rewritten.original_subject);
            rewritten.rewritten_subject = scrub.redact(&rewritten.rewritten_subject);
        }
    }
    // The paths before the private strip, so the Rust cascade knows which crate libraries the strip took.
    let before_strip: Vec<BTreeSet<String>> = if workflow.cascade_rust.is_some() {
        result
            .commits
            .iter()
            .map(|commit| commit.files.keys().cloned().collect())
            .collect()
    } else {
        Vec::new()
    };
    let private_stripped = match &private_strip {
        Some(private) => private.strip(
            &mut result.commits,
            &mut result.stripped,
            &mut result.files_stripped,
        ),
        None => 0,
    };
    // What the stripped files broke in the tree's Rust crates is repaired or stripped too.
    let mut cascade = CascadeSummary::default();
    if let Some(mode) = workflow.cascade_rust {
        for (index, commit) in result.commits.iter_mut().enumerate() {
            let mut events = Vec::new();
            // Every path of the origin commit, so an include of a file the origin never had is left alone.
            let origin_paths: Option<BTreeSet<String>> = git
                .list_paths(&origin, &commit.id)
                .ok()
                .map(|paths| paths.into_iter().collect());
            let summary = cascade_rust_in(
                &mut commit.files,
                &mut commit.modes,
                &before_strip[index],
                origin_paths.as_ref(),
                mode,
                &mut events,
            );
            cascade.files_stripped += summary.files_stripped;
            cascade.crates_stripped += summary.crates_stripped;
            cascade.members_pruned += summary.members_pruned;
            cascade.declarations_unlinked += summary.declarations_unlinked;
            cascade.manifest_edits += summary.manifest_edits;
            for event in events {
                if !commit.files.contains_key(&event.path) {
                    result.files_stripped.insert(event.path.clone());
                }
                result.stripped.push(StripEvent {
                    transform: "cascade.rust".to_string(),
                    commit: commit.id.clone(),
                    path: event.path,
                    detail: event.detail,
                });
            }
        }
    }
    if !result.refused.is_empty() {
        let shown: Vec<String> = result
            .refused
            .iter()
            .take(5)
            .map(|e| format!("{} in {} [{}]", e.detail, e.path, short(&e.commit)))
            .collect();
        return Err(anyhow!(
            "strip_references: {} leftover(s) after stripping; nothing written: {}",
            result.refused.len(),
            shown.join("; ")
        ));
    }

    let mut would_move = BTreeSet::new();
    for commit in &result.commits {
        would_move.extend(commit.files.keys().cloned());
    }
    let would_move: Vec<String> = would_move.into_iter().collect();
    let files_kept = would_move.len() as u64;
    let files_stripped = result.files_stripped.len() as u64;

    let verify_results = match (&workflow.verify, result.commits.last()) {
        (Some(verify), Some(head)) => run_verify(verify, base, head)?,
        _ => Vec::new(),
    };

    let mut notes = Vec::new();
    let origin_to_destination = if dry_run {
        dry_run_origin_map(&workflow, &result.commits, &head_rev)
    } else {
        let dest = dest
            .as_ref()
            .expect("non-dry migrate resolved a destination");
        let written = write_destination(git, dest, &workflow, &result.commits, &head_rev)?;
        if let Some(note) = point_bare_head(git, dest, &workflow)? {
            notes.push(note);
        }
        written
            .into_iter()
            .map(|(origin, dest_sha)| (origin, Some(dest_sha)))
            .collect()
    };

    let mut receipt = Receipt {
        workflow: workflow.name.clone(),
        dry_run,
        commits_read,
        commits_written: result.commits.len() as u64,
        files_kept,
        files_stripped,
        commits_dropped: result.drops.len() as u64,
        origin_range: OriginRange {
            since: workflow.origin.since.clone(),
            ref_name: workflow.origin.ref_name.clone(),
            resolved_since,
            resolved_ref: Some(resolved_ref.clone()),
        },
        last_rev,
        bounded,
        would_move: would_move.clone(),
        drops: Receipt::from_drops(&result.drops),
        origin_to_destination,
        destination: dest.as_ref().filter(|_| !dry_run).map(|path| {
            fs::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .display()
                .to_string()
        }),
        push_branch: Some(workflow.destination.push.clone()),
        seconds: None,
        model_hook: workflow
            .transformations
            .iter()
            .any(|t| matches!(t, Transformation::RewriteMessages { hook: Some(_), .. }))
            .then(|| "stubbed".to_string()),
        private_patterns: private_patterns_path(path, &workflow).map(|file| {
            fs::canonicalize(&file)
                .unwrap_or(file)
                .display()
                .to_string()
        }),
        lint_allow: workflow
            .lint_allow
            .iter()
            .map(|allow| ReceiptLintAllow {
                paths: allow.paths.clone(),
                patterns: allow.patterns.clone(),
            })
            .collect(),
        lint_ids: workflow.lint_references().0,
        private_scrub: private_scrub
            .as_ref()
            .map(|scrub| scrub.file.display().to_string()),
        private_scrub_rule_count: private_scrub.as_ref().map(|scrub| scrub.rules.len() as u64),
        transform_events: BTreeMap::new(),
        paths_moved: 0,
        paths_stayed: 0,
        cascade_rust: workflow.cascade_rust.map(|mode| mode.as_str().to_string()),
        verify: verify_results.clone(),
    };
    let mut moved: BTreeSet<&str> = folded.moved.iter().map(String::as_str).collect();
    let mut stayed: BTreeSet<&str> = folded.stayed.iter().map(String::as_str).collect();
    receipt.transform_events.clone_from(&folded.by_transform);
    for event in &result.stripped {
        *receipt
            .transform_events
            .entry(event.transform.clone())
            .or_default() += 1;
        if event.detail.starts_with("moved to ") || event.detail.starts_with("renamed to ") {
            moved.insert(event.path.as_str());
        } else if event.detail.starts_with("stays: target taken") {
            stayed.insert(event.path.as_str());
        }
    }
    receipt.paths_moved = moved.len() as u64;
    receipt.paths_stayed = stayed.len() as u64;

    writeln!(out, "workflow: {}", workflow.name)?;
    writeln!(out, "mode: {}", workflow.mode.as_str())?;
    writeln!(out, "dry-run: {dry_run}")?;
    writeln!(
        out,
        "origin: {} @ {}",
        origin.display(),
        workflow.origin.ref_name
    )?;
    match &dest {
        Some(dest) if !dry_run => writeln!(out, "destination: {}", dest.display())?,
        _ => writeln!(out, "destination: (not written)")?,
    }
    writeln!(
        out,
        "model hook: reserved, not wired (messages use the first-line fallback)"
    )?;
    writeln!(out, "commits read: {commits_read}")?;
    if commits_read == 0 {
        writeln!(out, "up to date")?;
    }
    if bounded {
        writeln!(
            out,
            "bounded: walked {commits_read} of {commits_in_range}; last walked {}",
            receipt.last_rev.as_deref().map(short).unwrap_or("(none)")
        )?;
    }
    writeln!(out, "commits written: {}", receipt.commits_written)?;
    if private_strip.is_some() {
        writeln!(out, "private strip: {}", plural(private_stripped, "file"))?;
    }
    if workflow.cascade_rust.is_some() {
        writeln!(
            out,
            "rust cascade: {} ({}), {} unlinked, {} pruned, {}",
            plural(cascade.files_stripped, "file"),
            plural(cascade.crates_stripped, "crate"),
            plural(cascade.declarations_unlinked, "declaration"),
            plural(cascade.members_pruned, "workspace member"),
            plural(cascade.manifest_edits, "manifest edit"),
        )?;
    }
    if let Some(verify) = &workflow.verify {
        if result.commits.is_empty() {
            writeln!(out, "verify: skipped (no commit to verify)")?;
        } else {
            let passed = verify_results.iter().filter(|v| v.ok).count();
            writeln!(out, "verify: {passed} of {} passed", verify.commands.len())?;
        }
    }
    for note in &notes {
        writeln!(out, "{note}")?;
    }
    writeln!(out)?;

    let moved_heading = if dry_run { "would move:" } else { "moved:" };
    writeln!(out, "{moved_heading}")?;
    if would_move.is_empty() {
        writeln!(out, "  (none)")?;
    } else {
        for path in &would_move {
            writeln!(out, "  {path}")?;
        }
    }
    writeln!(out)?;

    writeln!(out, "stripped:")?;
    if result.stripped.is_empty() && folded.total() == 0 {
        writeln!(out, "  (none)")?;
    } else {
        if folded.total() > 0 {
            writeln!(
                out,
                "  earlier windows, counted not listed: {} events",
                folded.total()
            )?;
            for (transform, count) in &folded.by_transform {
                writeln!(out, "    {transform}: {count}")?;
            }
        }
        for event in &result.stripped {
            writeln!(
                out,
                "  {}: {} ({}) [{}]",
                event.transform,
                event.path,
                event.detail,
                short(&event.commit)
            )?;
        }
    }
    writeln!(out)?;

    writeln!(out, "rewritten subjects:")?;
    if result.rewritten.is_empty() {
        if result.commits.is_empty() {
            writeln!(out, "  (none)")?;
        } else {
            for commit in &result.commits {
                writeln!(
                    out,
                    "  {}  {}",
                    short(&commit.id),
                    first_line(&commit.message)
                )?;
            }
        }
    } else {
        for item in &result.rewritten {
            writeln!(out, "  {}  {}", short(&item.commit), item.rewritten_subject)?;
            if item.original_subject != item.rewritten_subject {
                writeln!(out, "      was: {}", item.original_subject)?;
            }
        }
    }
    writeln!(out)?;

    if !result.drops.is_empty() {
        writeln!(out, "drops:")?;
        for drop in &result.drops {
            writeln!(out, "  {}  {}", short(&drop.commit), drop.reason)?;
        }
        writeln!(out)?;
    }

    let receipt_path = receipt_path(path, &workflow);
    if let Some(parent) = receipt_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("{}: cannot create receipt directory", parent.display()))?;
    }
    receipt.seconds = Some((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    let json = receipt.to_json().context("receipt: serialize JSON")?;
    fs::write(&receipt_path, json)
        .with_context(|| format!("{}: cannot write receipt", receipt_path.display()))?;
    writeln!(out, "receipt: {}", receipt_path.display())?;
    Ok(receipt_path)
}

/// `[verify]`: writes `head`'s tree into the verify folder and runs each command there, in order, before
/// anything is written. A command's output goes to stderr. With `on_fail = "refuse"` the first failure ends the run.
fn run_verify(
    verify: &Verify,
    base: &Path,
    head: &copybera_rules::Commit,
) -> Result<Vec<ReceiptVerify>> {
    use std::os::fd::AsFd;
    use std::process::{Command, Stdio};

    let mut _temp = None;
    let dir = match &verify.dir {
        Some(dir) => {
            let dir = resolve(base, dir);
            if dir.exists()
                && fs::read_dir(&dir)
                    .with_context(|| format!("verify.dir: cannot read {}", dir.display()))?
                    .next()
                    .is_some()
            {
                return Err(anyhow!(
                    "verify.dir: {} is not empty; move it aside first",
                    dir.display()
                ));
            }
            fs::create_dir_all(&dir)
                .with_context(|| format!("verify.dir: cannot create {}", dir.display()))?;
            dir
        }
        None => {
            let temp = tempfile::Builder::new()
                .prefix("copybera-verify-")
                .tempdir()
                .context("verify: cannot create a temporary folder")?;
            let dir = temp.path().to_path_buf();
            _temp = Some(temp);
            dir
        }
    };
    for (path, bytes) in &head.files {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("verify: cannot create {}", parent.display()))?;
        }
        match head.modes.get(path).map(String::as_str) {
            Some("120000") => {
                let link = String::from_utf8_lossy(bytes).into_owned();
                std::os::unix::fs::symlink(&link, &target)
                    .with_context(|| format!("verify: cannot link {}", target.display()))?;
            }
            mode => {
                fs::write(&target, bytes)
                    .with_context(|| format!("verify: cannot write {}", target.display()))?;
                if mode == Some("100755") {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
                }
            }
        }
    }
    let total = verify.commands.len();
    let mut results = Vec::with_capacity(total);
    for (index, argv) in verify.commands.iter().enumerate() {
        let label = argv.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
        eprintln!("verify {}/{total}: {}", index + 1, argv.join(" "));
        let started = Instant::now();
        let stdout = std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::inherit());
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&dir)
            .envs(verify.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(stdout)
            .status();
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let (ok, code) = match &status {
            Ok(status) => (status.success(), status.code()),
            Err(err) => {
                eprintln!(
                    "verify {}/{total}: cannot run {}: {err}",
                    index + 1,
                    argv[0]
                );
                (false, None)
            }
        };
        eprintln!(
            "verify {}/{total}: {} ({seconds} s)",
            index + 1,
            if ok { "ok" } else { "failed" }
        );
        results.push(ReceiptVerify {
            command: argv.clone(),
            ok,
            code,
            seconds,
        });
        if !ok && verify.refuse {
            let why = code.map_or_else(|| "did not exit".to_string(), |c| format!("exit {c}"));
            return Err(anyhow!(
                "verify: command {} of {total} ({label}) failed ({why}); nothing written; the tree is in {}",
                index + 1,
                if _temp.is_some() {
                    "a removed temporary folder".to_string()
                } else {
                    dir.display().to_string()
                }
            ));
        }
    }
    Ok(results)
}

fn write_destination(
    git: &Git,
    dest: &Path,
    workflow: &Workflow,
    commits: &[copybera_rules::Commit],
    resolved_ref: &str,
) -> Result<BTreeMap<String, String>> {
    let mut origin_to_destination = BTreeMap::new();
    if commits.is_empty() {
        return Ok(origin_to_destination);
    }
    let fetch = dest_ref(&workflow.destination.fetch);
    let push = dest_ref(&workflow.destination.push);
    let mut parent = git.rev_parse_optional(dest, &fetch)?;
    let (mut files, mut modes) = match &parent {
        Some(sha) => git.load_tree_with_modes(dest, sha)?,
        None => (BTreeMap::new(), BTreeMap::new()),
    };
    let mut last_sha = None;
    for commit in commits {
        files = merge_destination(&files, &commit.files, &workflow.destination_paths);
        modes = merge_destination(&modes, &commit.modes, &workflow.destination_paths);
        modes.retain(|path, _| files.contains_key(path));
        let origin_id = match workflow.mode {
            Mode::Squash => resolved_ref,
            Mode::Iterative => commit.id.as_str(),
        };
        let message = dest_message(&commit.message, origin_id);
        let sha = git.write_commit(
            dest,
            parent.as_deref(),
            TreeRef {
                files: &files,
                modes: &modes,
            },
            &message,
            Author {
                name: &commit.author_name,
                email: &commit.author_email,
                date: commit.author_date.as_deref(),
            },
            Author {
                name: &workflow.authoring.default.name,
                email: &workflow.authoring.default.email,
                date: None,
            },
        )?;
        origin_to_destination.insert(origin_id.to_string(), sha.clone());
        parent = Some(sha.clone());
        last_sha = Some(sha);
    }
    if let Some(sha) = last_sha {
        git.update_ref(dest, &push, &sha)?;
    }
    Ok(origin_to_destination)
}

/// A bare destination made with `git init --bare` points HEAD at `master` while the workflow
/// pushes `main`. After a push, an unborn HEAD is pointed at the push
/// branch; a HEAD that names a branch that exists is left alone.
fn point_bare_head(git: &Git, dest: &Path, workflow: &Workflow) -> Result<Option<String>> {
    if !git.is_bare(dest) {
        return Ok(None);
    }
    let push = dest_ref(&workflow.destination.push);
    if git.rev_parse_optional(dest, &push)?.is_none() {
        return Ok(None);
    }
    let head = git.symbolic_head(dest)?;
    if head.as_deref() == Some(push.as_str()) {
        return Ok(None);
    }
    if let Some(named) = &head {
        if git.rev_parse_optional(dest, named)?.is_some() {
            return Ok(None);
        }
    }
    git.set_symbolic_head(dest, &push)?;
    Ok(Some(format!(
        "destination HEAD: {push} (was {}, unborn)",
        head.as_deref().unwrap_or("detached")
    )))
}

fn dry_run_origin_map(
    workflow: &Workflow,
    commits: &[copybera_rules::Commit],
    resolved_ref: &str,
) -> BTreeMap<String, Option<String>> {
    let mut map = BTreeMap::new();
    if commits.is_empty() {
        return map;
    }
    match workflow.mode {
        Mode::Squash => {
            map.insert(resolved_ref.to_string(), None);
        }
        Mode::Iterative => {
            for commit in commits {
                map.insert(commit.id.clone(), None);
            }
        }
    }
    map
}

fn merge_destination<V: Clone>(
    parent: &BTreeMap<String, V>,
    incoming: &BTreeMap<String, V>,
    filter: &PathFilter,
) -> BTreeMap<String, V> {
    let mut tree = parent.clone();
    tree.retain(|path, _| !filter.allows(path));
    for (path, value) in incoming {
        tree.insert(path.clone(), value.clone());
    }
    tree
}

fn dest_message(message: &str, origin_id: &str) -> String {
    format!(
        "{}\n\nCopybera-RevId: {origin_id}\nGitOrigin-RevId: {origin_id}\n",
        message.trim_end(),
    )
}

fn dest_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_string()
    } else {
        format!("refs/heads/{name}")
    }
}

/// What `copybera lint` walks and reads: a destination or a receipt, a range or a workflow, and a private-patterns
/// file.
pub struct LintArgs<'a> {
    pub dest: Option<&'a Path>,
    pub from_receipt: Option<&'a Path>,
    pub range: Option<&'a str>,
    pub workflow: Option<&'a Path>,
    pub private: Option<&'a Path>,
    /// Reference ids in messages and file contents are kept on purpose (also the workflow's `[lint] references`).
    pub keep_references: bool,
    /// Count every finding per check instead of stopping at the first.
    pub count: bool,
}

/// The lint's findings: the first one refuses, or (`--count`) every one is counted per check by hits and files.
struct Tally {
    count: bool,
    rows: BTreeMap<String, (usize, BTreeSet<String>)>,
}

impl Tally {
    fn hit(&mut self, check: String, file: &str, refusal: impl FnOnce() -> String) -> Result<()> {
        if !self.count {
            return Err(anyhow!(refusal()));
        }
        let row = self.rows.entry(check).or_default();
        row.0 += 1;
        row.1.insert(file.to_string());
        Ok(())
    }
}

pub fn lint(args: LintArgs<'_>, git: &Git, cwd: &Path, out: &mut dyn Write) -> Result<()> {
    let LintArgs {
        dest,
        from_receipt,
        range,
        workflow: workflow_path,
        private,
        keep_references,
        count,
    } = args;
    let mut tally = Tally {
        count,
        rows: BTreeMap::new(),
    };
    // The private patterns, from --private, else the workflow's `[lint] private`, else the receipt's.
    let mut private_file = private.map(|path| resolve(cwd, path.to_string_lossy().as_ref()));
    let (dest, spec, ids, workflow, allowances, references) = if let Some(receipt_path) =
        from_receipt
    {
        let receipt_path = resolve(cwd, receipt_path.to_string_lossy().as_ref());
        let receipt = read_receipt(&receipt_path)?;
        if receipt.dry_run {
            return Err(anyhow!("lint: receipt is from a dry-run"));
        }
        let destination = receipt
            .destination
            .as_deref()
            .ok_or_else(|| anyhow!("lint: receipt has no destination"))?;
        let push_branch = receipt
            .push_branch
            .as_deref()
            .ok_or_else(|| anyhow!("lint: receipt has no push_branch"))?;
        let dest = resolve(receipt_path.parent().unwrap_or(cwd), destination);
        let spec = dest_ref(push_branch);
        if private_file.is_none() {
            private_file = receipt.private_patterns.as_deref().map(PathBuf::from);
        }
        // A receipt names the commits this run wrote, but its push branch is the current
        // public destination. Walk both: a reset, divergence, or later leak on that branch
        // must not let receipt-directed lint report a clean export.
        let ids = git
            .rev_list_range(&dest, &spec, false)
            .with_context(|| format!("lint: cannot walk {spec}"))?
            .into_iter()
            .chain(
                receipt
                    .origin_to_destination
                    .values()
                    .filter_map(Clone::clone),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let allowances = receipt
            .lint_allow
            .into_iter()
            .map(|allow| LintAllow::from_parts(allow.paths, allow.patterns).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;
        let references = receipt
            .lint_ids
            .iter()
            .map(|id| Regex::new(id).with_context(|| format!("lint: receipt id pattern {id:?}")))
            .collect::<Result<Vec<_>>>()?;
        (dest, spec, ids, None, allowances, references)
    } else {
        let dest = dest.ok_or_else(|| anyhow!("lint: destination is required"))?;
        let dest = resolve(cwd, dest.to_string_lossy().as_ref());
        let workflow_file = workflow_path
            .map(|path| resolve(cwd, path.to_string_lossy().as_ref()))
            .unwrap_or_else(|| cwd.join("workflow.toml"));
        let workflow = match workflow_path {
            Some(_) => Some(validate(&workflow_file)?),
            None => load_cwd_workflow(cwd)?,
        };
        if private_file.is_none() {
            private_file = workflow
                .as_ref()
                .and_then(|wf| private_patterns_path(&workflow_file, wf));
        }
        // The workflow's push branch, never HEAD: a bare destination's HEAD can name a branch the workflow never pushes.
        let spec = match (range, &workflow) {
            (Some(range), _) => range.to_string(),
            (None, Some(wf)) => dest_ref(&wf.destination.push),
            (None, None) => {
                return Err(anyhow!(
                    "lint: no branch to walk: pass --workflow <file> (its destination.push branch), run beside a \
                     workflow.toml, or name --range (the lint never walks HEAD)"
                ))
            }
        };
        let ids = git
            .rev_list_range(&dest, &spec, false)
            .with_context(|| format!("lint: cannot walk {spec}"))?;
        let allowances = workflow
            .as_ref()
            .map(|workflow| workflow.lint_allow.clone())
            .unwrap_or_default();
        let references = workflow
            .as_ref()
            .map(|workflow| workflow.lint_references().1)
            .unwrap_or_default();
        (dest, spec, ids, workflow, allowances, references)
    };
    if !dest.is_dir() {
        return Err(anyhow!("lint: {} is not a directory", dest.display()));
    }
    if !git.is_repo(&dest) {
        return Err(anyhow!("lint: {} is not a git repository", dest.display()));
    }
    let private = private_file
        .as_deref()
        .map(PrivatePatterns::load)
        .transpose()?;
    let keep_references =
        keep_references || workflow.as_ref().is_some_and(|wf| wf.lint_keep_references);
    let t = &mut tally;
    for id in &ids {
        let commit = git.load_commit(&dest, id)?;
        if !keep_references {
            refuse_lint_text(t, &references, "message", id, "<message>", &commit.message)?;
        }
        if let Some(private) = &private {
            private.refuse(
                t,
                "message",
                id,
                "<message>",
                "<message>",
                &commit.message,
                &[],
            )?;
            private.refuse(
                t,
                "identity",
                id,
                "<identity>",
                "<identity>",
                &git.identities(&dest, id)?,
                &[],
            )?;
        }
        for (path, bytes) in &commit.files {
            for found in references.iter().filter_map(|regex| regex.find(path)) {
                let found = found.as_str();
                t.hit("reference path".into(), path, || {
                    format!("lint: reference {found} in commit {id} path {path}")
                })?;
            }
            if let Some(wf) = &workflow {
                if excluded_by_workflow(wf, path) {
                    t.hit("excluded path".into(), path, || {
                        format!("lint: excluded path {path} in commit {id}")
                    })?;
                }
            }
            if let Some(private) = &private {
                // Filenames are public export data too. Check them separately because a private
                // name can be present even when a blob is binary or its contents are clean.
                private.refuse(
                    t,
                    "filename",
                    id,
                    path,
                    "<private filename>",
                    path,
                    &allowances,
                )?;
            }
            if let Some(kind) = copybera_rules::gate::audio_signature(bytes) {
                t.hit(format!("binary audio ({kind})"), path, || {
                    format!("lint: binary audio ({kind}) in commit {id} path {path}")
                })?;
            }
            if let Ok(text) = std::str::from_utf8(bytes) {
                if !keep_references {
                    refuse_lint_text(t, &references, "path", id, path, text)?;
                }
                if let Some(private) = &private {
                    private.refuse(t, "path", id, path, path, text, &allowances)?;
                }
            }
        }
    }
    if count {
        writeln!(out, "{:<24} {:>7} {:>7}", "check", "hits", "files")?;
        for (check, (hits, files)) in &tally.rows {
            writeln!(out, "{check:<24} {hits:>7} {:>7}", files.len())?;
        }
        let total: usize = tally.rows.values().map(|(hits, _)| hits).sum();
        if total > 0 {
            return Err(anyhow!("lint: {total} finding(s) in {spec}"));
        }
    }
    let private_note = match &private {
        Some(private) => format!(
            "; {} from {}",
            plural(private.regexes.len(), "private pattern"),
            private.file.display()
        ),
        None => String::new(),
    };
    writeln!(
        out,
        "ok: lint {} ({spec}, {} commits{private_note}{})",
        dest.display(),
        ids.len(),
        if keep_references {
            "; reference ids in text kept"
        } else {
            ""
        }
    )?;
    Ok(())
}

/// `copybera gate <workflow>`: the destination's push-branch tree scanned by class. Prints classes and
/// counts only and refuses on any finding.
pub fn gate(
    path: &Path,
    private: Option<&Path>,
    gate_file: Option<&Path>,
    list_files: bool,
    git: &Git,
    out: &mut dyn Write,
) -> Result<()> {
    let workflow = validate(path)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let (mut config, gate_base) = match gate_file {
        Some(file) => {
            let text = fs::read_to_string(file)
                .with_context(|| format!("gate: cannot read {}", file.display()))?;
            let doc: toml::Table = text
                .parse()
                .map_err(|err| anyhow!("gate: {} is not TOML: {err}", file.display()))?;
            let table = match doc.get("gate") {
                Some(toml::Value::Table(table)) => table,
                _ => return Err(anyhow!("gate: {} holds no [gate] table", file.display())),
            };
            let config = copybera_rules::gate::GateConfig::parse(table)
                .map_err(|err| anyhow!("gate: {}: {err}", file.display()))?;
            (config, file.parent().unwrap_or_else(|| Path::new(".")))
        }
        None => (workflow.gate.clone().unwrap_or_default(), base),
    };
    config.private_allow = workflow.lint_allow.clone();
    if let Some(file) = &config.networks_file {
        let file = resolve(gate_base, file);
        let text = fs::read_to_string(&file)
            .with_context(|| format!("gate: cannot read networks file {}", file.display()))?;
        config
            .add_networks(&text)
            .map_err(|err| anyhow!("gate: {} {err}", file.display()))?;
    }
    let private_file = private
        .map(Path::to_path_buf)
        .or_else(|| private_patterns_path(path, &workflow));
    let private = private_file
        .as_deref()
        .map(PrivatePatterns::load)
        .transpose()?;
    let dest_path = workflow
        .destination
        .location
        .path
        .as_ref()
        .ok_or_else(|| anyhow!("gate: destination.path is required"))?;
    let dest = resolve(base, dest_path);
    let spec = dest_ref(&workflow.destination.push);
    let sha = git
        .rev_parse(&dest, &spec)
        .with_context(|| format!("gate: cannot resolve {spec} in {}", dest.display()))?;
    let (files, _) = git.load_tree_with_modes(&dest, &sha)?;
    let regexes = private
        .as_ref()
        .map(|p| p.regexes.as_slice())
        .unwrap_or(&[]);
    // The dropped-notice check reads the origin's paths at its ref; without a readable origin it is skipped
    // and the header says so.
    let origin_paths = match (&config.licence, &workflow.origin.location.path) {
        (Some(licence), Some(origin)) if licence.dropped_notices => {
            let origin = resolve(base, origin);
            git.list_paths(&origin, &workflow.origin.ref_name).ok()
        }
        _ => None,
    };
    let report =
        copybera_rules::gate::scan_with_origin(&files, &config, regexes, origin_paths.as_deref());
    writeln!(
        out,
        "gate: {} ({spec} {}, {} files{})",
        dest.display(),
        short(&sha),
        report.files_scanned,
        match &private {
            Some(p) => format!("; {}", plural(p.regexes.len(), "private pattern")),
            None => "; private terms not read".to_string(),
        }
    )?;
    writeln!(out, "{:<14} {:>7} {:>7}", "class", "hits", "files")?;
    for class in copybera_rules::gate::CLASSES {
        let count = report.counts[class];
        writeln!(out, "{class:<14} {:>7} {:>7}", count.hits, count.files)?;
    }
    if let Some(licence) = &config.licence {
        let headers: usize = report
            .licences
            .iter()
            .filter(|(id, _)| !id.starts_with("file:"))
            .map(|(_, n)| n)
            .sum();
        let texts: usize = report
            .licences
            .iter()
            .filter(|(id, _)| id.starts_with("file:"))
            .map(|(_, n)| n)
            .sum();
        writeln!(
            out,
            "licences: {headers} SPDX headers, {texts} licence files, {} carried upstreams, dropped notices {}",
            report.carried_sources,
            match (licence.dropped_notices, &origin_paths) {
                (false, _) => "not checked (off)",
                (true, None) => "not checked (origin unreadable)",
                (true, Some(_)) => "checked against the origin",
            }
        )?;
    }
    if list_files {
        for finding in &report.files {
            writeln!(out, "{} {} {}", finding.class, finding.count, finding.path)?;
        }
    }
    let findings = report.findings();
    writeln!(out, "findings: {findings}")?;
    if findings > 0 {
        return Err(anyhow!("gate: {findings} finding(s)"));
    }
    Ok(())
}

/// The leak lint's patterns: names, hosts and personal facts that must never reach a public
/// destination. One regex per line in a file outside git; blank lines and `#` lines are skipped. A finding names the
/// pattern's number, the commit, the path and the line, never the text it matched.
pub(crate) struct PrivatePatterns {
    file: PathBuf,
    pub(crate) regexes: Vec<Regex>,
}

impl PrivatePatterns {
    pub(crate) fn load(file: &Path) -> Result<Self> {
        let text = fs::read_to_string(file)
            .with_context(|| format!("lint: cannot read private patterns {}", file.display()))?;
        let mut regexes = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // The regex error quotes the pattern, which is private: name the line only.
            let regex = Regex::new(line)
                .map_err(|_| anyhow!("{} line {}: invalid regex", file.display(), index + 1))?;
            regexes.push(regex);
        }
        if regexes.is_empty() {
            return Err(anyhow!(
                "lint: private patterns {} holds no pattern",
                file.display()
            ));
        }
        Ok(Self {
            file: file.to_path_buf(),
            regexes,
        })
    }

    /// The first pattern (1-based) that matches `path` or a line of its text; a file that is not UTF-8 is read as
    /// the gate reads it (Mach-O signature masked, decoded lossily), so a stripping export gates 0.
    fn first_hit(&self, path: &str, bytes: &[u8]) -> Option<usize> {
        if let Some(at) = self.regexes.iter().position(|r| r.is_match(path)) {
            return Some(at + 1);
        }
        let data = copybera_rules::gate::masked(bytes);
        let text = String::from_utf8_lossy(&data);
        self.regexes
            .iter()
            .position(|r| text.lines().any(|line| r.is_match(line)))
            .map(|at| at + 1)
    }

    /// `[lint] private_action = "strip"`: drops every file a pattern hits, by name or by a line of text, from every
    /// commit, with one `lint.private` event naming the first pattern's number. A path a pattern hits is shown by its
    /// top folder only (when that folder is clean). Returns the number of distinct paths dropped.
    fn strip(
        &self,
        commits: &mut [copybera_rules::Commit],
        stripped: &mut Vec<StripEvent>,
        files_stripped: &mut BTreeSet<String>,
    ) -> usize {
        let mut dropped = BTreeSet::new();
        for commit in commits {
            let hits: Vec<(String, usize)> = commit
                .files
                .iter()
                .filter_map(|(path, bytes)| Some((path.clone(), self.first_hit(path, bytes)?)))
                .collect();
            for (path, pattern) in hits {
                commit.files.remove(&path);
                commit.modes.remove(&path);
                let shown = if self.regexes.iter().any(|r| r.is_match(&path)) {
                    match path.split_once('/') {
                        Some((top, _)) if !self.regexes.iter().any(|r| r.is_match(top)) => {
                            format!("{top}/{}", copybera_rules::gate::WITHHELD)
                        }
                        _ => copybera_rules::gate::WITHHELD.to_string(),
                    }
                } else {
                    path.clone()
                };
                stripped.push(StripEvent {
                    transform: "lint.private".to_string(),
                    commit: commit.id.clone(),
                    path: shown,
                    detail: format!("path dropped (private pattern {pattern})"),
                });
                files_stripped.insert(path.clone());
                dropped.insert(path);
            }
        }
        dropped.len()
    }

    #[allow(clippy::too_many_arguments)]
    fn refuse(
        &self,
        tally: &mut Tally,
        kind: &str,
        commit: &str,
        path: &str,
        display_path: &str,
        text: &str,
        allowances: &[LintAllow],
    ) -> Result<()> {
        for (number, line) in text.lines().enumerate() {
            for (index, regex) in self.regexes.iter().enumerate() {
                let pattern = index + 1;
                if regex.is_match(line)
                    && !allowances.iter().any(|allow| allow.allows(path, pattern))
                {
                    tally.hit(format!("private pattern {pattern}"), path, || {
                        format!(
                            "lint: private term (pattern {pattern} of {}) in commit {commit} {kind} {display_path} line {}",
                            self.file.display(),
                            number + 1
                        )
                    })?;
                }
            }
        }
        Ok(())
    }
}

/// Private post-transformation rules: one `regex<TAB>replacement` per active line. The rule text stays private;
/// parse errors and strip events name rule numbers only.
pub(crate) struct ScrubRules {
    file: PathBuf,
    rules: Vec<(Regex, String)>,
}

impl ScrubRules {
    pub(crate) fn load(file: &Path) -> Result<Self> {
        let text = fs::read_to_string(file)
            .with_context(|| format!("scrub: cannot read private rules {}", file.display()))?;
        let mut rules = Vec::new();
        for raw_line in text.split('\n') {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let number = rules.len() + 1;
            let (pattern, replacement) = line.split_once('\t').ok_or_else(|| {
                anyhow!(
                    "scrub: {} rule {number}: expected regex and replacement",
                    file.display()
                )
            })?;
            if pattern.is_empty() {
                return Err(anyhow!(
                    "scrub: {} rule {number}: empty regex",
                    file.display()
                ));
            }
            let regex = Regex::new(pattern)
                .map_err(|_| anyhow!("scrub: {} rule {number}: invalid regex", file.display()))?;
            rules.push((regex, replacement.to_string()));
        }
        if rules.is_empty() {
            return Err(anyhow!(
                "scrub: private rules {} hold no rule",
                file.display()
            ));
        }
        Ok(Self {
            file: fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf()),
            rules,
        })
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        self.rules
            .iter()
            .fold(text.to_string(), |current, (regex, replacement)| {
                regex.replace_all(&current, replacement).into_owned()
            })
    }

    fn apply(&self, commits: &mut [copybera_rules::Commit], stripped: &mut Vec<StripEvent>) {
        for commit in commits {
            for (index, (regex, replacement)) in self.rules.iter().enumerate() {
                let number = index + 1;
                let message_hits = regex.find_iter(&commit.message).count();
                if message_hits > 0 {
                    commit.message = regex.replace_all(&commit.message, replacement).into_owned();
                    stripped.push(StripEvent {
                        transform: "scrub.private".to_string(),
                        commit: commit.id.clone(),
                        path: "<message>".to_string(),
                        detail: format!("rule {number}: {message_hits} replacements"),
                    });
                }
                let paths: Vec<String> = commit.files.keys().cloned().collect();
                for path in paths {
                    let Some(bytes) = commit.files.get(&path) else {
                        continue;
                    };
                    let Ok(text) = std::str::from_utf8(bytes) else {
                        continue;
                    };
                    let hits = regex.find_iter(text).count();
                    if hits > 0 {
                        let next = regex.replace_all(text, replacement).into_owned();
                        commit
                            .files
                            .insert(path.clone(), Blob::from(next.into_bytes()));
                        stripped.push(StripEvent {
                            transform: "scrub.private".to_string(),
                            commit: commit.id.clone(),
                            path,
                            detail: format!("rule {number}: {hits} replacements"),
                        });
                    }
                }
            }
        }
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The workflow's `[lint] private` file, read from the workflow file's folder when relative.
pub(crate) fn private_patterns_path(workflow_path: &Path, workflow: &Workflow) -> Option<PathBuf> {
    let named = workflow.lint_private.as_deref()?;
    let base = workflow_path.parent().unwrap_or_else(|| Path::new("."));
    Some(resolve(base, named))
}

/// The workflow's `[scrub] private` file, read from the workflow file's folder when relative.
pub(crate) fn scrub_rules_path(workflow_path: &Path, workflow: &Workflow) -> Option<PathBuf> {
    let named = workflow.scrub_private.as_deref()?;
    let base = workflow_path.parent().unwrap_or_else(|| Path::new("."));
    Some(resolve(base, named))
}

fn load_cwd_workflow(cwd: &Path) -> Result<Option<Workflow>> {
    let path = cwd.join("workflow.toml");
    match fs::read_to_string(&path) {
        Ok(text) => parse_workflow(&text).map(Some).map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("{}: cannot read workflow", path.display()))
        }
    }
}

fn excluded_by_workflow(workflow: &Workflow, path: &str) -> bool {
    if workflow.origin_paths.exclude_set.is_match(path)
        || workflow.destination_paths.exclude_set.is_match(path)
    {
        return true;
    }
    workflow.transformations.iter().any(|t| match t {
        Transformation::StripPaths { matcher, .. } => matcher.is_match(path),
        _ => false,
    })
}

/// Every match of the workflow's reference patterns (`[lint] ids`, else its `strip_references` patterns) in a message
/// or a file's text is one `reference` finding.
fn refuse_lint_text(
    tally: &mut Tally,
    references: &[Regex],
    kind: &str,
    commit: &str,
    path: &str,
    text: &str,
) -> Result<()> {
    for regex in references {
        for found in regex.find_iter(text) {
            let found = found.as_str();
            tally.hit("reference".into(), path, || {
                format!("lint: reference {found} in commit {commit} {kind} {path}")
            })?;
        }
    }
    Ok(())
}

pub fn doctor(path: &Path, git: &Git, out: &mut dyn Write) -> Result<()> {
    let workflow = validate(path)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let origin = workflow
        .origin
        .location
        .path
        .as_deref()
        .map(|value| resolve(base, value));
    let dest = workflow
        .destination
        .location
        .path
        .as_deref()
        .map(|value| resolve(base, value));
    let mut failed = false;

    let origin_readable = origin
        .as_deref()
        .is_some_and(|value| value.is_dir() && git.is_repo(value));
    doctor_check(
        out,
        &mut failed,
        "origin readable",
        origin_readable,
        origin
            .as_deref()
            .map(|value| value.display().to_string())
            .unwrap_or_else(|| "origin.path is not local".to_string()),
    )?;
    // Only tracked files count: untracked ones (scratch files in the origin checkout) are never
    // read by a migration, which walks commits.
    let origin_clean = origin_readable
        && origin
            .as_deref()
            .is_some_and(|value| git.has_tracked_changes(value).is_ok_and(|changed| !changed));
    doctor_check(
        out,
        &mut failed,
        "origin clean",
        origin_clean,
        if origin_clean {
            "clean (tracked files; untracked files are not counted)"
        } else {
            "tracked files modified or staged, or unreadable (untracked files are not counted)"
        }
        .to_string(),
    )?;

    let dest_bare = dest
        .as_deref()
        .is_some_and(|value| value.is_dir() && git.is_bare(value));
    doctor_check(
        out,
        &mut failed,
        "destination bare",
        dest_bare,
        dest.as_deref()
            .map(|value| value.display().to_string())
            .unwrap_or_else(|| "destination.path is not local".to_string()),
    )?;
    let remotes = if dest_bare {
        git.remotes(dest.as_deref().expect("bare destination exists"))
            .unwrap_or_else(|_| vec!["unreadable".to_string()])
    } else {
        vec!["unreadable".to_string()]
    };
    doctor_check(
        out,
        &mut failed,
        "destination remotes",
        remotes.is_empty(),
        if remotes.is_empty() {
            "none".to_string()
        } else {
            remotes.join(", ")
        },
    )?;
    let push = dest_ref(&workflow.destination.push);
    let head_matches = dest_bare
        && dest.as_deref().is_some_and(|value| {
            git.symbolic_head(value).ok().flatten().as_deref() == Some(push.as_str())
        });
    doctor_check(
        out,
        &mut failed,
        "destination HEAD",
        head_matches,
        if head_matches {
            push.clone()
        } else {
            format!("expected {push}")
        },
    )?;

    let receipt_path = receipt_path(path, &workflow);
    if !receipt_path.exists() {
        doctor_check(
            out,
            &mut failed,
            "receipt last_rev",
            true,
            "skipped (no receipt)".to_string(),
        )?;
    } else {
        let receipt_status = read_receipt(&receipt_path).and_then(|receipt| {
            let last_rev = receipt
                .last_rev
                .ok_or_else(|| anyhow!("receipt has no last_rev"))?;
            let origin = origin
                .as_deref()
                .ok_or_else(|| anyhow!("origin is not local"))?;
            if git.is_ancestor(origin, &last_rev, &workflow.origin.ref_name)? {
                Ok(last_rev)
            } else {
                Err(anyhow!("{last_rev} is not in {}", workflow.origin.ref_name))
            }
        });
        match receipt_status {
            Ok(last_rev) => doctor_check(out, &mut failed, "receipt last_rev", true, last_rev)?,
            Err(error) => doctor_check(
                out,
                &mut failed,
                "receipt last_rev",
                false,
                error.to_string(),
            )?,
        }
    }
    // A workflow that names its leak lint's patterns must be able to read them (fail closed).
    if let Some(file) = private_patterns_path(path, &workflow) {
        match PrivatePatterns::load(&file) {
            Ok(private) => doctor_check(
                out,
                &mut failed,
                "private patterns",
                true,
                format!(
                    "{} ({})",
                    plural(private.regexes.len(), "pattern"),
                    file.display()
                ),
            )?,
            Err(error) => doctor_check(
                out,
                &mut failed,
                "private patterns",
                false,
                format!("{error:#}"),
            )?,
        }
    }
    doctor_check(
        out,
        &mut failed,
        "lint allowances",
        true,
        plural(workflow.lint_allow.len(), "allowance"),
    )?;
    if let Some(file) = scrub_rules_path(path, &workflow) {
        match ScrubRules::load(&file) {
            Ok(scrub) => doctor_check(
                out,
                &mut failed,
                "private scrub",
                true,
                format!("{} ({})", plural(scrub.rules.len(), "rule"), file.display()),
            )?,
            Err(error) => doctor_check(
                out,
                &mut failed,
                "private scrub",
                false,
                format!("{error:#}"),
            )?,
        }
    }
    if failed {
        Err(anyhow!("doctor: one or more checks failed"))
    } else {
        Ok(())
    }
}

/// `copybera status`: one row per workflow, read from its receipt (the last run's last_rev, bounded, commit counts,
/// seconds and subjects) and from the doctor's verdict. `paths` are workflow files or folders of them (`*.toml`).
pub fn status(paths: &[PathBuf], git: &Git, cwd: &Path, out: &mut dyn Write) -> Result<()> {
    let mut files = Vec::new();
    for path in paths {
        let path = resolve(cwd, path.to_string_lossy().as_ref());
        if path.is_dir() {
            let mut found: Vec<PathBuf> = fs::read_dir(&path)
                .with_context(|| format!("status: cannot read {}", path.display()))?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|file| file.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            found.sort();
            files.extend(found);
        } else if path.is_file() {
            files.push(path);
        } else {
            return Err(anyhow!(
                "status: {} is not a workflow file or folder",
                path.display()
            ));
        }
    }

    let mut rows = vec![[
        "workflow".to_string(),
        "last_rev".to_string(),
        "bounded".to_string(),
        "commits".to_string(),
        "seconds".to_string(),
        "subjects".to_string(),
        "doctor".to_string(),
    ]];
    for file in &files {
        rows.push(status_row(file, git));
    }
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0))
        .collect();
    for row in &rows {
        let mut line = String::new();
        for (column, cell) in row.iter().enumerate() {
            if column + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<width$}  ", width = widths[column]));
            }
        }
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}

/// Reprints the status table at `interval` until `count` tables have been emitted, if bounded.
pub fn watch_status(
    paths: &[PathBuf],
    interval: Duration,
    count: Option<usize>,
    git: &Git,
    cwd: &Path,
    out: &mut dyn Write,
) -> Result<()> {
    let mut printed = 0;
    loop {
        status(paths, git, cwd, out)?;
        out.flush()?;
        printed += 1;
        if count.is_some_and(|count| printed >= count) {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn status_row(file: &Path, git: &Git) -> [String; 7] {
    let workflow = match validate(file) {
        Ok(workflow) => workflow,
        Err(error) => {
            let name = file
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let dash = || "-".to_string();
            return [
                name,
                dash(),
                dash(),
                dash(),
                dash(),
                dash(),
                format!("invalid: {error:#}"),
            ];
        }
    };
    let mut report = Vec::new();
    let verdict = match doctor(file, git, &mut report) {
        Ok(()) => "ok".to_string(),
        Err(_) => {
            let failing: Vec<String> = String::from_utf8_lossy(&report)
                .lines()
                .filter_map(|line| line.strip_prefix("FAIL "))
                .filter_map(|line| line.split(':').next())
                .map(str::to_string)
                .collect();
            format!("FAIL {}", failing.join(", "))
        }
    };
    let receipt_file = receipt_path(file, &workflow);
    let (last_rev, bounded, commits, seconds, subjects) = if !receipt_file.exists() {
        let none = || "-".to_string();
        ("no receipt".to_string(), none(), none(), none(), none())
    } else {
        match read_receipt(&receipt_file) {
            Ok(receipt) => (
                receipt
                    .last_rev
                    .as_deref()
                    .map(|rev| rev.chars().take(10).collect())
                    .unwrap_or_else(|| "-".to_string()),
                receipt.bounded.to_string(),
                format!("{}/{}", receipt.commits_read, receipt.commits_written),
                receipt
                    .seconds
                    .map(|seconds| format!("{seconds:.1}"))
                    .unwrap_or_else(|| "-".to_string()),
                match receipt.model_hook.as_deref() {
                    Some("stubbed") => "fallback".to_string(),
                    Some(other) => other.to_string(),
                    None => "-".to_string(),
                },
            ),
            Err(_) => {
                let none = || "-".to_string();
                ("unreadable".to_string(), none(), none(), none(), none())
            }
        }
    };
    [
        workflow.name,
        last_rev,
        bounded,
        commits,
        seconds,
        subjects,
        verdict,
    ]
}

fn doctor_check(
    out: &mut dyn Write,
    failed: &mut bool,
    check: &str,
    ok: bool,
    detail: String,
) -> Result<()> {
    if !ok {
        *failed = true;
    }
    writeln!(out, "{} {check}: {detail}", if ok { "ok" } else { "FAIL" })?;
    Ok(())
}

fn read_receipt(path: &Path) -> Result<Receipt> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("{}: cannot read receipt", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{}: receipt is not JSON", path.display()))
}

pub fn print_receipt(path: &Path, out: &mut dyn Write) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("{}: cannot read receipt", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{}: receipt is not JSON", path.display()))?;
    writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

fn receipt_path(workflow_path: &Path, workflow: &Workflow) -> PathBuf {
    if let Some(named) = &workflow.receipt {
        let base = workflow_path.parent().unwrap_or_else(|| Path::new("."));
        return resolve(base, named);
    }
    let stem = workflow_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("workflow");
    workflow_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.receipt.json"))
}

pub(crate) fn resolve(base: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

pub(crate) fn short(id: &str) -> &str {
    if id.len() >= 7 {
        &id[..7]
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use super::*;

    #[derive(Default)]
    struct FlushRecorder {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlushRecorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn watched_status_flushes_every_table() {
        let temp = tempfile::tempdir().unwrap();
        let workflow = temp.path().join("workflow.toml");
        std::fs::write(&workflow, "not a workflow").unwrap();
        let mut out = FlushRecorder::default();

        watch_status(
            &[workflow],
            Duration::ZERO,
            Some(2),
            &Git::new(None),
            temp.path(),
            &mut out,
        )
        .unwrap();

        assert_eq!(out.flushes, 2);
    }
}
