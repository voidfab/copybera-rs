use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_copybera"))
}

struct Fixture {
    _tmp: TempDir,
    home: PathBuf,
    root: PathBuf,
    origin: PathBuf,
    dest: PathBuf,
    _workflow: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(".gitconfig"),
            "[user]\n    name = Fixture\n    email = fixture@example.invalid\n[init]\n    defaultBranch = main\n",
        )
        .unwrap();

        let origin = root.join("origin");
        let dest = root.join("dest");
        git(
            &home,
            None,
            &["init", "-b", "main", origin.to_str().unwrap()],
        );
        git(
            &home,
            None,
            &["init", "--bare", "-b", "main", dest.to_str().unwrap()],
        );

        write(&origin, "keep.txt", "hello\n");
        write(&origin, "src/lib.rs", "pub fn keep() {}\n");
        git_in(&home, &origin, &["add", "keep.txt", "src/lib.rs"]);
        git_in(&home, &origin, &["commit", "-m", "initial keep"]);

        write(&origin, "keep.txt", "hello logbook\n");
        write(&origin, ".logbook/item.md", "tracker PROJ-aaabbb\n");
        write(&origin, "secret/x.txt", "nope\n");
        git_in(
            &home,
            &origin,
            &["add", "keep.txt", ".logbook/item.md", "secret/x.txt"],
        );
        git_in(
            &home,
            &origin,
            &["commit", "-m", "PROJ-aaabbb add tracker\n\nlogbook details"],
        );

        write(&origin, ".logbook/item.md", "only tracker\n");
        git_in(&home, &origin, &["add", ".logbook/item.md"]);
        git_in(&home, &origin, &["commit", "-m", "tracker only"]);

        let workflow = root.join("workflow.toml");
        write_workflow(&workflow, "squash", "dest");

        Self {
            _tmp: tmp,
            home,
            root,
            origin,
            dest,
            _workflow: workflow,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(bin());
        cmd.env("HOME", &self.home);
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.current_dir(&self.root);
        cmd
    }
}

fn write_workflow(path: &Path, mode: &str, dest: &str) {
    fs::write(
        path,
        format!(
            r#"
[workflow]
name = "fixture"
mode = "{mode}"
dry_run = false
receipt = "fixture.receipt.json"

[origin]
path = "origin"
ref = "main"

[destination]
path = "{dest}"
fetch = "main"
push = "main"

[paths.origin]
include = ["**"]
exclude = ["secret/**"]

[paths.destination]
include = ["**"]
exclude = []

[authoring]
default = {{ name = "Fixture Authors", email = "authors@example.invalid" }}

[[authoring.map]]
from = {{ name = "Fixture", email = "fixture@example.invalid" }}
to = {{ name = "Public Fixture", email = "public@example.invalid" }}

[[transformations]]
type = "strip_paths"
globs = [".logbook/**"]

[[transformations]]
type = "strip_references"
patterns = ["PROJ-[0-9a-f]{{6}}", "logbook"]

[[transformations]]
type = "rewrite_messages"
hook = "low_cost_model"
prompt_budget_tokens = 512
fallback = "first_line_strip_references"

[[transformations]]
type = "drop_empty_commits"
"#
        ),
    )
    .unwrap();
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn git(home: &Path, repo: Option<&Path>, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.env("HOME", home);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_AUTHOR_NAME", "Fixture");
    cmd.env("GIT_AUTHOR_EMAIL", "fixture@example.invalid");
    cmd.env("GIT_COMMITTER_NAME", "Fixture");
    cmd.env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    if let Some(repo) = repo {
        cmd.arg("-C").arg(repo);
    }
    cmd.args(args);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_in(home: &Path, repo: &Path, args: &[&str]) {
    git(home, Some(repo), args);
}

fn git_out(home: &Path, repo: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.env("HOME", home);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.arg("-C").arg(repo).args(args);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}

#[test]
fn validate_accepts_a_workflow() {
    let fx = Fixture::new();
    let output = fx
        .cmd()
        .args(["validate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ok: fixture (squash)"), "{stdout}");
}

#[test]
fn validate_names_the_refusing_key() {
    let fx = Fixture::new();
    fs::write(
        fx.root.join("bad.toml"),
        "[workflow]\nmode = \"iterative\"\n",
    )
    .unwrap();
    let output = fx.cmd().args(["validate", "bad.toml"]).output().unwrap();
    assert!(!output.status.success());
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(err.contains("workflow.name"), "{err}");
}

#[test]
fn migrate_dry_run_walks_the_fixture_repo_under_fixture_home() {
    let fx = Fixture::new();
    let dest_before = git_head(&fx.home, &fx.dest);

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    assert!(stdout.contains("dry-run: true"), "{stdout}");
    assert!(
        stdout.contains("model hook: reserved, not wired"),
        "{stdout}"
    );
    assert!(stdout.contains("would move:"), "{stdout}");
    let moved = section(&stdout, "would move:", "stripped:");
    assert!(moved.contains("keep.txt"), "{stdout}");
    assert!(moved.contains("src/lib.rs"), "{stdout}");
    assert!(!moved.contains("secret/x.txt"), "{stdout}");
    assert!(!moved.contains(".logbook/"), "{stdout}");
    assert!(stdout.contains("rewritten subjects:"), "{stdout}");
    assert!(stdout.contains("add tracker"), "{stdout}");
    assert!(stdout.contains("was: PROJ-aaabbb add tracker"), "{stdout}");
    assert!(stdout.contains("stripped:"), "{stdout}");
    assert!(stdout.contains(".logbook/item.md"), "{stdout}");

    let receipt_path = fx.root.join("fixture.receipt.json");
    assert!(receipt_path.is_file(), "receipt not written");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["dry_run"], true);
    assert_eq!(receipt["workflow"], "fixture");
    assert!(receipt["commits_read"].as_u64().unwrap() >= 2);
    let origin_head = git_out(&fx.home, &fx.origin, &["rev-parse", "HEAD"]);
    assert!(
        receipt["origin_to_destination"][&origin_head].is_null(),
        "dry-run omits destination shas: {receipt}"
    );
    assert!(receipt["would_move"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "keep.txt"));
    assert!(receipt.get("rewritten_subjects").is_none());
    let receipt_text = fs::read_to_string(&receipt_path).unwrap();
    assert!(
        !receipt_text.contains("PROJ-aaabbb"),
        "receipt must not store unstripped original subjects: {receipt_text}"
    );
    assert!(
        !receipt_text.contains("original_subject"),
        "receipt is counts, paths, range, last-rev, drops; not rewritten messages: {receipt_text}"
    );

    let dest_after = git_head(&fx.home, &fx.dest);
    assert_eq!(dest_before, dest_after, "dry-run wrote the destination");
    assert!(
        git_head(&fx.home, &fx.origin).is_some(),
        "origin should still have commits"
    );

    let printed = fx
        .cmd()
        .args(["receipt", "fixture.receipt.json"])
        .output()
        .unwrap();
    assert!(printed.status.success());
    let body = String::from_utf8_lossy(&printed.stdout);
    assert!(body.contains("\"dry_run\": true"), "{body}");
}

#[test]
fn migrate_batches_filtered_blobs_and_limits_origin_walk() {
    let fx = Fixture::new();
    let shim_dir = fx.root.join("git-shim");
    let log = fx.root.join("git-invocations.log");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$COPYBERA_GIT_LOG\"\nexec \"$COPYBERA_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(shim_dir.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set")),
    ))
    .unwrap();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git is on PATH");

    let output = fx
        .cmd()
        .env("PATH", path)
        .env("COPYBERA_GIT_LOG", &log)
        .env("COPYBERA_REAL_GIT", real_git)
        .args([
            "migrate",
            "workflow.toml",
            "--dry-run",
            "--max-commits",
            "2",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.root.join("fixture.receipt.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["commits_read"], 2);
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("loading origin commit "))
            .count(),
        2,
        "stderr={stderr}"
    );

    let invocation_log = fs::read_to_string(log).unwrap();
    let invocations = invocation_log.lines().count();
    assert!(
        invocations <= 5 + (2 * 3),
        "expected at most one log, tree, and batch read per commit; got {invocations} git invocations: {invocation_log}"
    );
}

#[test]
fn migrate_reads_each_origin_blob_once_with_one_batch() {
    let fx = Fixture::new();
    let shim_dir = fx.root.join("git-shim");
    let log = fx.root.join("git-invocations.log");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$COPYBERA_GIT_LOG\"\nexec \"$COPYBERA_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(shim_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH is set"),
    )))
    .unwrap();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git is on PATH");

    let output = fx
        .cmd()
        .env("PATH", path)
        .env("COPYBERA_GIT_LOG", &log)
        .env("COPYBERA_REAL_GIT", real_git)
        .args(["migrate", "workflow.toml", "--dry-run"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}");
    assert!(
        stderr.ends_with("blobs read 5 for 8 files\n"),
        "stderr={stderr}"
    );
    let invocation_log = fs::read_to_string(log).unwrap();
    assert_eq!(
        invocation_log
            .lines()
            .filter(|line| line.contains("cat-file --batch"))
            .count(),
        1,
        "expected one cat-file batch: {invocation_log}"
    );
}

/// More history for the window tests: tracker-only commits (dropped), a path moving between commits, and one
/// subject per kept commit.
fn grow_history(fx: &Fixture, extra: usize) {
    for index in 0..extra {
        if index % 3 == 2 {
            write(
                &fx.origin,
                ".logbook/item.md",
                &format!("tracker {index}\n"),
            );
            git_in(&fx.home, &fx.origin, &["add", ".logbook/item.md"]);
        } else {
            write(&fx.origin, "keep.txt", &format!("hello {index}\n"));
            write(
                &fx.origin,
                &format!("src/f{index}.rs"),
                &format!("pub fn f{index}() {{}}\n"),
            );
            git_in(&fx.home, &fx.origin, &["add", "keep.txt", "src"]);
        }
        git_in(
            &fx.home,
            &fx.origin,
            &["commit", "-m", &format!("change {index}")],
        );
    }
}

fn squash_with_window(window: Option<&str>) -> (String, String, String) {
    let fx = Fixture::new();
    grow_history(&fx, 40);
    let mut cmd = fx.cmd();
    if let Some(window) = window {
        cmd.env("COPYBERA_TREE_WINDOW", window);
    }
    let output = cmd.args(["migrate", "workflow.toml"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "stderr={stderr}");
    let tree = git_out(&fx.home, &fx.dest, &["rev-parse", "HEAD^{tree}"]);
    // The origin ids in the trailers differ between fixtures (commit dates); the subjects are what the walk decides.
    let full = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B"]);
    let message = full.split("\n\nCopybera-RevId").next().unwrap().to_string();
    (tree, message, stderr)
}

#[test]
fn a_squash_walk_holds_one_window_of_trees_and_writes_what_one_pass_writes() {
    let (whole_tree, whole_message, _) = squash_with_window(Some("1000"));
    let (tree, message, stderr) = squash_with_window(Some("4"));
    assert_eq!(tree, whole_tree);
    assert_eq!(message, whole_message);
    let windows: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("tree window "))
        .collect();
    assert_eq!(windows.len(), 11, "43 commits in windows of 4: {stderr}");
    assert!(
        windows.iter().all(|line| {
            let held: usize = line.rsplit(' ').nth(1).unwrap().parse().unwrap();
            held <= 4
        }),
        "{stderr}"
    );
    let (one_tree, one_message, _) = squash_with_window(Some("1"));
    assert_eq!(one_tree, whole_tree);
    assert_eq!(one_message, whole_message);
}

// A squash of a long history joins thousands of subjects into one message; Linux refuses a single argument over
// 128 KiB, so the message reaches git commit-tree on stdin, not as -m.
#[test]
fn a_squash_message_over_128_kib_is_written() {
    let fx = Fixture::new();
    let subject = "a long subject ".repeat(10_000);
    write(&fx.origin, "keep.txt", "long\n");
    git_in(&fx.home, &fx.origin, &["add", "keep.txt"]);
    let message_file = fx.root.join("long-message.txt");
    fs::write(&message_file, &subject).unwrap();
    git_in(
        &fx.home,
        &fx.origin,
        &["commit", "-q", "-F", message_file.to_str().unwrap()],
    );
    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "stderr={}",
        &stderr[stderr.len().saturating_sub(400)..]
    );
    let message = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B"]);
    assert!(message.len() > 128 * 1024, "{}", message.len());
    assert!(message.contains(subject.trim_end()));
}

// A window whose trees each hold their own copy of every file, and copy every edited file again per commit, holds
// gigabytes for a large tree. A window's commits hold one copy of a file they carry
// (and one of an edit they share), so a window holds about one tree, not 32.
#[test]
fn a_squash_window_holds_about_one_tree_not_one_per_commit() {
    let fx = Fixture::new();
    let chunk = "a line that names logbook\n".repeat(1260);
    for index in 0..8 {
        write(
            &fx.origin,
            &format!("big/b{index}.txt"),
            &format!("{index}\n{chunk}"),
        );
    }
    git_in(&fx.home, &fx.origin, &["add", "big"]);
    git_in(
        &fx.home,
        &fx.origin,
        &["commit", "-m", "eight files of 32 KiB"],
    );
    grow_history(&fx, 40);
    let tree_bytes = 8 * (chunk.len() + 2);
    let output = fx
        .cmd()
        .env("COPYBERA_TREE_WINDOW", "32")
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "stderr={stderr}");
    let held: Vec<(usize, usize)> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("window "))
        .map(|line| {
            let words: Vec<&str> = line.split(' ').collect();
            (words[3].parse().unwrap(), words[9].parse().unwrap())
        })
        .collect();
    assert_eq!(
        held.len(),
        2,
        "44 commits in windows of 32 name their bytes: {stderr}"
    );
    for (loaded, transformed) in held {
        assert!(
            loaded < 2 * tree_bytes,
            "loaded {loaded} for a {tree_bytes}-byte tree: {stderr}"
        );
        assert!(
            transformed < 2 * tree_bytes,
            "transformed {transformed} for a {tree_bytes}-byte tree: {stderr}"
        );
    }
}

// A history with a path that moves partway (an `acme-admin` folder appears at commit 20), files
// that name it, deleted files, tracker-only commits that the strip empties, and one name-following pair rewrite. The
// windowed walk must write what the one-pass walk writes: same tree, same subjects, same receipt counts.
const MOVE_THEN_FOLLOW: &str = "
[[transformations]]
type = \"move_paths\"
pairs = [[\"acme\", \"rebrand\"]]

[[transformations]]
type = \"replace_pairs\"
on = \"both\"
follow_moves = true
pairs = [[\"zzzz\", \"zzzz2\"]]
";

const FOLLOW_THEN_MOVE: &str = "
[[transformations]]
type = \"replace_pairs\"
on = \"both\"
follow_moves = true
pairs = [[\"zzzz\", \"zzzz2\"]]

[[transformations]]
type = \"move_paths\"
pairs = [[\"acme\", \"rebrand\"]]
";

fn moving_history(fx: &Fixture, workflow_tail: &str) {
    let base = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!("{base}{workflow_tail}"),
    )
    .unwrap();
    for index in 0..40 {
        if index % 3 == 2 {
            write(
                &fx.origin,
                ".logbook/item.md",
                &format!("tracker {index}\n"),
            );
            git_in(&fx.home, &fx.origin, &["add", ".logbook/item.md"]);
        } else {
            write(
                &fx.origin,
                "keep.txt",
                &format!("hello {index} acme-admin\n"),
            );
            write(
                &fx.origin,
                &format!("src/f{index}.rs"),
                &format!("// acme-admin {index}\n"),
            );
            git_in(&fx.home, &fx.origin, &["add", "keep.txt", "src"]);
        }
        if index == 20 {
            write(
                &fx.origin,
                "apps/acme-admin/main.rs",
                "fn main() {} // acme-admin\n",
            );
            git_in(&fx.home, &fx.origin, &["add", "apps"]);
        }
        if index == 25 {
            git_in(
                &fx.home,
                &fx.origin,
                &["rm", "-q", "src/f0.rs", "src/f1.rs"],
            );
        }
        git_in(
            &fx.home,
            &fx.origin,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("change {index} about acme-admin"),
            ],
        );
    }
}

struct Walked {
    tree: String,
    subjects: String,
    receipt: serde_json::Value,
}

fn walk_moving_history(workflow_tail: &str, window: &str) -> Walked {
    let fx = Fixture::new();
    moving_history(&fx, workflow_tail);
    let output = fx
        .cmd()
        .env("COPYBERA_TREE_WINDOW", window)
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "window {window}: stderr={stderr}");
    let full = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B"]);
    let mut receipt = read_json(&fx.root.join("fixture.receipt.json"));
    for volatile in [
        "seconds",
        "origin_to_destination",
        "last_rev",
        "origin_range",
        "destination",
    ] {
        receipt.as_object_mut().unwrap().remove(volatile);
    }
    // Commit ids differ between fixtures (dates); the reasons and their order are what the walk decides.
    let reasons: Vec<serde_json::Value> = receipt["drops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|drop| drop["reason"].clone())
        .collect();
    receipt["drops"] = serde_json::Value::Array(reasons);
    Walked {
        tree: git_out(&fx.home, &fx.dest, &["rev-parse", "HEAD^{tree}"]),
        subjects: full.split("\n\nCopybera-RevId").next().unwrap().to_string(),
        receipt,
    }
}

fn assert_windows_agree(workflow_tail: &str) -> Walked {
    let whole = walk_moving_history(workflow_tail, "1000");
    for window in ["1", "4"] {
        let walked = walk_moving_history(workflow_tail, window);
        assert_eq!(walked.tree, whole.tree, "window {window}: tree");
        assert_eq!(walked.subjects, whole.subjects, "window {window}: subjects");
        // The receipt's per-transformation counts and path totals are the sums over the windows.
        assert_eq!(walked.receipt, whole.receipt, "window {window}: receipt");
    }
    whole
}

#[test]
fn a_windowed_walk_rewrites_early_messages_with_names_a_later_move_records() {
    let whole = assert_windows_agree(MOVE_THEN_FOLLOW);
    assert!(
        whole.subjects.contains("change 0 about rebrand-admin"),
        "an early subject follows the later move: {}",
        whole.subjects
    );
}

#[test]
fn a_windowed_walk_does_not_apply_moves_a_later_transformation_records_to_earlier_ones() {
    let whole = assert_windows_agree(FOLLOW_THEN_MOVE);
    assert!(
        whole.subjects.contains("change 39 about acme-admin"),
        "a rewrite ahead of the move never sees its names: {}",
        whole.subjects
    );
    assert!(
        !whole.subjects.contains("rebrand-admin"),
        "{}",
        whole.subjects
    );
}

#[test]
fn a_commit_the_strip_empties_at_a_window_start_is_dropped_as_in_one_pass() {
    let whole = assert_windows_agree(MOVE_THEN_FOLLOW);
    // Every third commit is tracker-only: dropped, wherever a window starts.
    assert_eq!(whole.receipt["commits_dropped"], 13, "{}", whole.receipt);
}

#[test]
fn a_bad_tree_window_is_named_and_the_default_used() {
    for bad in ["0", "-3", "many"] {
        let fx = Fixture::new();
        let output = fx
            .cmd()
            .env("COPYBERA_TREE_WINDOW", bad)
            .args(["migrate", "workflow.toml", "--dry-run"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("COPYBERA_TREE_WINDOW"), "{bad}: {stderr}");
        assert!(stderr.contains("using 32"), "{bad}: {stderr}");
    }
}

#[test]
fn a_windowed_walk_counts_the_earlier_windows_events_instead_of_listing_them() {
    let fx = Fixture::new();
    grow_history(&fx, 40);
    let output = fx
        .cmd()
        .env("COPYBERA_TREE_WINDOW", "4")
        .args(["migrate", "workflow.toml", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("earlier windows, counted not listed:"),
        "{stdout}"
    );
    assert!(
        stderr.contains("a blob shared across windows counts once per window"),
        "{stderr}"
    );
    let listed = section(&stdout, "stripped:", "rewritten subjects:");
    assert!(listed.lines().count() < 40, "{listed}");
}

#[test]
fn migrate_squash_writes_the_bare_destination() {
    let fx = Fixture::new();
    let origin_head = git_out(&fx.home, &fx.origin, &["rev-parse", "HEAD"]);

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    assert!(stdout.contains("dry-run: false"), "{stdout}");
    assert!(stdout.contains("moved:"), "{stdout}");
    assert!(
        stdout.contains("model hook: reserved, not wired"),
        "{stdout}"
    );
    let moved = section(&stdout, "moved:", "stripped:");
    assert!(moved.contains("keep.txt"), "{stdout}");
    assert!(moved.contains("src/lib.rs"), "{stdout}");
    assert!(!moved.contains("secret/x.txt"), "{stdout}");
    assert!(!moved.contains(".logbook/"), "{stdout}");
    assert!(stdout.contains("add tracker"), "{stdout}");

    let keep = git_out(&fx.home, &fx.dest, &["show", "HEAD:keep.txt"]);
    assert_eq!(keep.trim_end(), "hello");
    assert!(!keep.contains("logbook"), "{keep}");
    let lib = git_out(&fx.home, &fx.dest, &["show", "HEAD:src/lib.rs"]);
    assert_eq!(lib.trim_end(), "pub fn keep() {}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "HEAD"],
    );
    assert!(listing.contains("keep.txt"), "{listing}");
    assert!(listing.contains("src/lib.rs"), "{listing}");
    assert!(!listing.contains(".logbook"), "{listing}");
    assert!(!listing.contains("secret"), "{listing}");

    let body = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B"]);
    assert!(body.contains("initial keep"), "{body}");
    assert!(body.contains("add tracker"), "{body}");
    assert!(!body.contains("PROJ-aaabbb"), "{body}");
    assert!(!body.contains("logbook"), "{body}");
    assert!(
        body.contains(&format!("Copybera-RevId: {origin_head}")),
        "{body}"
    );
    assert!(
        body.contains(&format!("GitOrigin-RevId: {origin_head}")),
        "{body}"
    );

    let author = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%an <%ae>"]);
    assert_eq!(author, "Fixture Authors <authors@example.invalid>");

    let count = git_out(&fx.home, &fx.dest, &["rev-list", "--count", "HEAD"]);
    assert_eq!(count, "1");

    let receipt_path = fx.root.join("fixture.receipt.json");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["dry_run"], false);
    assert_eq!(receipt["commits_written"], 1);
    assert_eq!(receipt["commits_dropped"], 1);
    let mapped = receipt["origin_to_destination"][&origin_head]
        .as_str()
        .expect("origin_to_destination dest sha");
    assert_eq!(mapped, git_out(&fx.home, &fx.dest, &["rev-parse", "HEAD"]));
    assert!(receipt["would_move"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "keep.txt"));
    let receipt_text = fs::read_to_string(&receipt_path).unwrap();
    assert!(
        !receipt_text.contains("PROJ-aaabbb"),
        "receipt must not store unstripped original subjects: {receipt_text}"
    );
    assert!(receipt.get("rewritten_subjects").is_none());
}

#[test]
fn migrate_iterative_writes_mapped_authors() {
    let fx = Fixture::new();
    write_workflow(&fx.root.join("workflow.toml"), "iterative", "dest");

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("mode: iterative"), "{stdout}");

    let count = git_out(&fx.home, &fx.dest, &["rev-list", "--count", "HEAD"]);
    assert_eq!(count, "2");

    let authors = git_out(
        &fx.home,
        &fx.dest,
        &["log", "--reverse", "--format=%an <%ae>"],
    );
    for line in authors.lines() {
        assert_eq!(line, "Public Fixture <public@example.invalid>");
    }

    let messages = git_out(&fx.home, &fx.dest, &["log", "--reverse", "--format=%s"]);
    assert!(messages.contains("initial keep"), "{messages}");
    assert!(messages.contains("add tracker"), "{messages}");
    assert!(!messages.contains("PROJ-aaabbb"), "{messages}");

    let keep = git_out(&fx.home, &fx.dest, &["show", "HEAD:keep.txt"]);
    assert_eq!(keep.trim_end(), "hello");
    assert!(!keep.contains("logbook"), "{keep}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "HEAD"],
    );
    assert!(!listing.contains(".logbook"), "{listing}");
    assert!(!listing.contains("secret"), "{listing}");

    let trailers = git_out(&fx.home, &fx.dest, &["log", "--format=%b"]);
    assert!(trailers.contains("Copybera-RevId:"), "{trailers}");
    assert!(trailers.contains("GitOrigin-RevId:"), "{trailers}");

    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.root.join("fixture.receipt.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["dry_run"], false);
    assert_eq!(receipt["commits_written"], 2);
    assert_eq!(receipt["commits_dropped"], 1);
    let map = receipt["origin_to_destination"]
        .as_object()
        .expect("origin_to_destination");
    assert_eq!(map.len(), 2);
    for (_origin, dest_sha) in map {
        assert!(dest_sha.as_str().is_some(), "{receipt}");
    }
}

#[test]
fn migrate_refuses_a_dirty_destination() {
    let fx = Fixture::new();
    let dirty = fx.root.join("dirty-dest");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", dirty.to_str().unwrap()],
    );
    git_in(&fx.home, &dirty, &["commit", "--allow-empty", "-m", "seed"]);
    write(&dirty, "dirty.txt", "uncommitted\n");
    write_workflow(&fx.root.join("workflow.toml"), "squash", "dirty-dest");

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("destination.path"), "{err}");
    assert!(err.contains("dirty"), "{err}");
    let listing = git_out(&fx.home, &dirty, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!listing.contains("keep.txt"), "{listing}");
    assert_eq!(
        git_out(&fx.home, &dirty, &["rev-list", "--count", "HEAD"]),
        "1"
    );
}

#[test]
fn migrate_refuses_a_non_git_origin() {
    let fx = Fixture::new();
    let not_git = fx.root.join("not-git");
    fs::create_dir_all(&not_git).unwrap();
    fs::write(not_git.join("file.txt"), "nope\n").unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        r#"
[workflow]
name = "fixture"
mode = "squash"
[origin]
path = "not-git"
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
default = { name = "Fixture Authors", email = "authors@example.invalid" }
"#,
    )
    .unwrap();

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("origin.path"), "{err}");
    assert!(err.contains("not a git repository"), "{err}");
}

#[test]
fn migrate_walks_first_parent_only() {
    let fx = Fixture::new();
    git_in(&fx.home, &fx.origin, &["checkout", "-b", "side"]);
    write(&fx.origin, "side.txt", "side\n");
    git_in(&fx.home, &fx.origin, &["add", "side.txt"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "side only"]);
    git_in(&fx.home, &fx.origin, &["checkout", "main"]);
    git_in(
        &fx.home,
        &fx.origin,
        &["merge", "--no-ff", "-m", "merge side", "side"],
    );
    write_workflow(&fx.root.join("workflow.toml"), "iterative", "dest");

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    let messages = git_out(&fx.home, &fx.dest, &["log", "--format=%s"]);
    assert!(
        !messages.lines().any(|s| s == "side only"),
        "first-parent walk leaked the side commit: {messages}"
    );
    assert!(messages.contains("merge side"), "{messages}");
}

#[test]
fn migrate_unmapped_author_becomes_default() {
    let fx = Fixture::new();
    write(&fx.origin, "extra.txt", "extra\n");
    git_in(&fx.home, &fx.origin, &["add", "extra.txt"]);
    git_in_as(
        &fx.home,
        &fx.origin,
        "Stranger",
        "stranger@internal.example",
        &["commit", "-m", "unmapped author"],
    );
    write_workflow(&fx.root.join("workflow.toml"), "iterative", "dest");

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    let authors = git_out(&fx.home, &fx.dest, &["log", "--format=%an <%ae>"]);
    assert!(
        authors.contains("Fixture Authors <authors@example.invalid>"),
        "{authors}"
    );
    assert!(!authors.contains("stranger@internal.example"), "{authors}");
}

#[test]
fn migrate_rewrites_or_drops_trailer_emails() {
    let fx = Fixture::new();
    fs::write(
        fx.root.join("workflow.toml"),
        r#"
[workflow]
name = "fixture"
mode = "iterative"
dry_run = false
receipt = "fixture.receipt.json"

[origin]
path = "origin"
ref = "main"

[destination]
path = "dest"
fetch = "main"
push = "main"

[paths.origin]
include = ["**"]
exclude = []

[paths.destination]
include = ["**"]
exclude = []

[authoring]
default = { name = "Fixture Authors", email = "authors@example.invalid" }

[[authoring.map]]
from = { name = "Fixture", email = "fixture@example.invalid" }
to = { name = "Public Fixture", email = "public@example.invalid" }
"#,
    )
    .unwrap();
    write(&fx.origin, "signed.txt", "ok\n");
    git_in(&fx.home, &fx.origin, &["add", "signed.txt"]);
    let msg = fx.origin.join("COMMIT_MSG");
    fs::write(
        &msg,
        "signed off\n\nSigned-off-by: Fixture <fixture@example.invalid>\nCo-authored-by: Bob <bob@internal.example>\n",
    )
    .unwrap();
    git_in(
        &fx.home,
        &fx.origin,
        &["commit", "-F", msg.to_str().unwrap()],
    );

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    let body = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Signed-off-by: Public Fixture <public@example.invalid>"),
        "{body}"
    );
    assert!(!body.contains("bob@internal.example"), "{body}");
    assert!(!body.contains("Co-authored-by"), "{body}");
}

#[test]
fn migrate_reads_gitorigin_revid() {
    let fx = Fixture::new();
    let first = git_out(
        &fx.home,
        &fx.origin,
        &["rev-list", "--max-parents=0", "HEAD"],
    );
    let labeled = fx.root.join("labeled");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", labeled.to_str().unwrap()],
    );
    git_in(
        &fx.home,
        &labeled,
        &[
            "commit",
            "--allow-empty",
            "-m",
            &format!("seed\n\nGitOrigin-RevId: {first}\n"),
        ],
    );
    write_workflow(&fx.root.join("workflow.toml"), "iterative", "labeled");

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.root.join("fixture.receipt.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["commits_read"], 2);
}

#[test]
fn lint_accepts_a_clean_destination() {
    let fx = Fixture::new();
    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = fx.cmd().args(["lint", "dest"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("ok: lint"), "{stdout}");
}

#[test]
fn lint_refuses_reference_ids_and_excluded_path() {
    let fx = Fixture::new();
    let leaky = fx.root.join("leaky");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", leaky.to_str().unwrap()],
    );
    git_in(
        &fx.home,
        &leaky,
        &["commit", "--allow-empty", "-m", "clean"],
    );
    let clean = git_out(&fx.home, &leaky, &["rev-parse", "HEAD"]);

    git_in(
        &fx.home,
        &leaky,
        &["commit", "--allow-empty", "-m", "PROJ-aaabbb leaked"],
    );
    let output = fx.cmd().args(["lint", "leaky"]).output().unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("reference PROJ-aaabbb"), "{err}");

    let range_ok = fx
        .cmd()
        .args(["lint", "leaky", "--range", &clean])
        .output()
        .unwrap();
    assert!(
        range_ok.status.success(),
        "range excluding the leak should pass: {}",
        String::from_utf8_lossy(&range_ok.stderr)
    );
    let range_bad = format!("{clean}..HEAD");
    let range_fail = fx
        .cmd()
        .args(["lint", "leaky", "--range", &range_bad])
        .output()
        .unwrap();
    assert!(!range_fail.status.success());
    assert!(
        String::from_utf8_lossy(&range_fail.stderr).contains("reference PROJ-aaabbb"),
        "{}",
        String::from_utf8_lossy(&range_fail.stderr)
    );

    let logbook = fx.root.join("logbook-dest");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", logbook.to_str().unwrap()],
    );
    write(&logbook, "note.txt", "see .logbook later\n");
    git_in(&fx.home, &logbook, &["add", "note.txt"]);
    git_in(&fx.home, &logbook, &["commit", "-m", "mention"]);
    let output = fx.cmd().args(["lint", "logbook-dest"]).output().unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("reference logbook in commit"), "{err}");

    let excluded = fx.root.join("excluded-dest");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", excluded.to_str().unwrap()],
    );
    write(&excluded, "secret/x.txt", "nope\n");
    git_in(&fx.home, &excluded, &["add", "secret/x.txt"]);
    git_in(&fx.home, &excluded, &["commit", "-m", "secret"]);
    let output = fx.cmd().args(["lint", "excluded-dest"]).output().unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("excluded path"), "{err}");
    assert!(err.contains("secret/x.txt"), "{err}");
}

fn git_in_as(home: &Path, repo: &Path, name: &str, email: &str, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.env("HOME", home);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_AUTHOR_NAME", name);
    cmd.env("GIT_AUTHOR_EMAIL", email);
    cmd.env("GIT_COMMITTER_NAME", name);
    cmd.env("GIT_COMMITTER_EMAIL", email);
    cmd.arg("-C").arg(repo).args(args);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text.find(start).unwrap_or(0);
    let rest = &text[from..];
    match rest.find(end) {
        Some(to) => &rest[..to],
        None => rest,
    }
}

#[test]
fn migrate_resume_matches_a_whole_run_and_refuses_invalid_receipts() {
    let resumed = Fixture::new();
    write(&resumed.origin, "final.txt", "final\n");
    git_in(&resumed.home, &resumed.origin, &["add", "final.txt"]);
    git_in(&resumed.home, &resumed.origin, &["commit", "-m", "final"]);
    let first = resumed
        .cmd()
        .args(["migrate", "workflow.toml", "--max-commits", "2"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let resumed_run = resumed
        .cmd()
        .args(["migrate", "workflow.toml", "--resume"])
        .output()
        .unwrap();
    assert!(
        resumed_run.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed_run.stderr)
    );

    let whole = Fixture::new();
    write(&whole.origin, "final.txt", "final\n");
    git_in(&whole.home, &whole.origin, &["add", "final.txt"]);
    git_in(&whole.home, &whole.origin, &["commit", "-m", "final"]);
    let whole_run = whole
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        whole_run.status.success(),
        "{}",
        String::from_utf8_lossy(&whole_run.stderr)
    );
    assert_eq!(
        git_out(&resumed.home, &resumed.dest, &["rev-parse", "main^{tree}"]),
        git_out(&whole.home, &whole.dest, &["rev-parse", "main^{tree}"])
    );

    let invalid = Fixture::new();
    let missing = invalid
        .cmd()
        .args(["migrate", "workflow.toml", "--resume"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let dry = invalid
        .cmd()
        .args([
            "migrate",
            "workflow.toml",
            "--dry-run",
            "--max-commits",
            "2",
        ])
        .output()
        .unwrap();
    assert!(dry.status.success());
    let dry_resume = invalid
        .cmd()
        .args(["migrate", "workflow.toml", "--resume"])
        .output()
        .unwrap();
    assert!(!dry_resume.status.success());
    assert!(String::from_utf8_lossy(&dry_resume.stderr).contains("dry-run"));

    let receipt_path = invalid.root.join("fixture.receipt.json");
    let mut receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
    receipt["dry_run"] = serde_json::Value::Bool(false);
    receipt["bounded"] = serde_json::Value::Bool(false);
    fs::write(&receipt_path, serde_json::to_string(&receipt).unwrap()).unwrap();
    let unbounded = invalid
        .cmd()
        .args(["migrate", "workflow.toml", "--resume"])
        .output()
        .unwrap();
    assert!(!unbounded.status.success());
    assert!(String::from_utf8_lossy(&unbounded.stderr).contains("bounded"));

    receipt["bounded"] = serde_json::Value::Bool(true);
    receipt["last_rev"] =
        serde_json::Value::String("0000000000000000000000000000000000000000".into());
    fs::write(&receipt_path, serde_json::to_string(&receipt).unwrap()).unwrap();
    let foreign = invalid
        .cmd()
        .args(["migrate", "workflow.toml", "--resume"])
        .output()
        .unwrap();
    assert!(!foreign.status.success());
    assert!(String::from_utf8_lossy(&foreign.stderr).contains("ancestor"));
}

#[test]
fn lint_from_receipt_uses_its_destination_and_rejects_dry_runs() {
    let fx = Fixture::new();
    let migrate = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        migrate.status.success(),
        "{}",
        String::from_utf8_lossy(&migrate.stderr)
    );
    let receipt_path = fx.root.join("fixture.receipt.json");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
    assert_eq!(
        receipt["destination"],
        fs::canonicalize(&fx.dest)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(receipt["push_branch"], "main");

    let lint = fx
        .cmd()
        .args(["lint", "--from-receipt", "fixture.receipt.json"])
        .output()
        .unwrap();
    assert!(
        lint.status.success(),
        "{}",
        String::from_utf8_lossy(&lint.stderr)
    );
    let conflict = fx
        .cmd()
        .args([
            "lint",
            "--from-receipt",
            "fixture.receipt.json",
            "--range",
            "HEAD",
        ])
        .output()
        .unwrap();
    assert!(!conflict.status.success());

    let mut dry: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
    dry["dry_run"] = serde_json::Value::Bool(true);
    fs::write(&receipt_path, serde_json::to_string(&dry).unwrap()).unwrap();
    let dry_lint = fx
        .cmd()
        .args(["lint", "--from-receipt", "fixture.receipt.json"])
        .output()
        .unwrap();
    assert!(!dry_lint.status.success());
    assert!(String::from_utf8_lossy(&dry_lint.stderr).contains("dry-run"));
}

#[test]
fn lint_from_receipt_walks_the_destination_push_branch() {
    let fx = Fixture::new();
    let migrate = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        migrate.status.success(),
        "{}",
        String::from_utf8_lossy(&migrate.stderr)
    );

    let leaky = fx.root.join("leaky-destination");
    git(
        &fx.home,
        None,
        &["clone", fx.dest.to_str().unwrap(), leaky.to_str().unwrap()],
    );
    write(&leaky, "leak.txt", "PROJ-aaabbb leaked\n");
    git_in(&fx.home, &leaky, &["add", "leak.txt"]);
    git_in(&fx.home, &leaky, &["commit", "-m", "leak"]);
    git_in(&fx.home, &leaky, &["push", "origin", "main"]);

    let lint = fx
        .cmd()
        .args(["lint", "--from-receipt", "fixture.receipt.json"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&lint.stderr);
    assert!(!lint.status.success(), "leaky push branch passed: {stderr}");
    assert!(stderr.contains("reference PROJ-aaabbb"), "{stderr}");
}

#[test]
fn doctor_reports_each_check_and_fails_on_a_changed_tracked_file_only() {
    let fx = Fixture::new();
    let migrate = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        migrate.status.success(),
        "{}",
        String::from_utf8_lossy(&migrate.stderr)
    );
    let healthy = fx.cmd().args(["doctor", "workflow.toml"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&healthy.stdout);
    assert!(
        healthy.status.success(),
        "{}",
        String::from_utf8_lossy(&healthy.stderr)
    );
    for check in [
        "origin readable",
        "origin clean",
        "destination bare",
        "destination remotes",
        "destination HEAD",
        "receipt last_rev",
    ] {
        assert!(stdout.contains(check), "missing {check}: {stdout}");
    }

    // an untracked file (a scratch file in the origin checkout) does not fail it, and it says so
    write(&fx.origin, "scratch.txt", "untracked\n");
    let untracked = fx.cmd().args(["doctor", "workflow.toml"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&untracked.stdout);
    assert!(untracked.status.success(), "{stdout}");
    assert!(
        stdout.contains("ok origin clean: clean (tracked files; untracked files are not counted)"),
        "{stdout}"
    );

    // a modified tracked file does
    write(&fx.origin, "keep.txt", "changed\n");
    let unhealthy = fx.cmd().args(["doctor", "workflow.toml"]).output().unwrap();
    assert!(!unhealthy.status.success());
    let stdout = String::from_utf8_lossy(&unhealthy.stdout);
    assert!(
        stdout.contains("FAIL origin clean: tracked files modified or staged"),
        "{stdout}"
    );

    // and so does a staged new file
    git_in(&fx.home, &fx.origin, &["checkout", "--", "keep.txt"]);
    git_in(&fx.home, &fx.origin, &["add", "scratch.txt"]);
    let staged = fx.cmd().args(["doctor", "workflow.toml"]).output().unwrap();
    assert!(!staged.status.success());
    assert!(String::from_utf8_lossy(&staged.stdout).contains("FAIL origin clean"));
}

fn git_head(home: &Path, repo: &Path) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.env("HOME", home);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "HEAD"]);
    let output = cmd.output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

// A stripped reference takes its punctuation; a bounded run claims only what it walked; the lint walks the push
// branch.

fn origin_ids(fx: &Fixture) -> Vec<String> {
    git_out(
        &fx.home,
        &fx.origin,
        &["rev-list", "--reverse", "--first-parent", "main"],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

fn master_headed_dest(fx: &Fixture, name: &str) -> PathBuf {
    let dest = fx.root.join(name);
    git(
        &fx.home,
        None,
        &["init", "--bare", "-b", "master", dest.to_str().unwrap()],
    );
    write_workflow(&fx.root.join("workflow.toml"), "squash", name);
    dest
}

#[test]
fn migrate_bounded_receipt_names_the_last_walked_commit() {
    let fx = Fixture::new();
    let ids = origin_ids(&fx);
    assert_eq!(ids.len(), 3);

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml", "--max-commits", "2"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.root.join("fixture.receipt.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["bounded"], true, "{receipt}");
    assert_eq!(receipt["last_rev"], ids[1].as_str(), "{receipt}");
    assert_eq!(receipt["origin_range"]["resolved_ref"], ids[2].as_str());
    let map = receipt["origin_to_destination"].as_object().unwrap();
    assert!(map.contains_key(&ids[1]), "{receipt}");
    assert!(!map.contains_key(&ids[2]), "{receipt}");

    let body = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B", "main"]);
    assert!(
        body.contains(&format!("Copybera-RevId: {}", ids[1])),
        "a resumed run must start after the last walked commit: {body}"
    );

    let full = fx
        .cmd()
        .args(["migrate", "workflow.toml", "--dry-run"])
        .output()
        .unwrap();
    assert!(full.status.success());
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.root.join("fixture.receipt.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["bounded"], false, "{receipt}");
}

#[test]
fn migrate_points_a_bare_destinations_head_at_the_push_branch() {
    let fx = Fixture::new();
    let dest = master_headed_dest(&fx, "dest-master");
    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");
    assert_eq!(
        git_out(&fx.home, &dest, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert!(git_head(&fx.home, &dest).is_some());
}

#[test]
fn lint_rejects_an_invalid_default_workflow() {
    let fx = Fixture::new();
    fs::write(fx.root.join("workflow.toml"), "destination = [").unwrap();

    let output = fx.cmd().args(["lint", "dest"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr={stderr}");
    assert!(stderr.contains("TOML parse error"), "stderr={stderr}");
}

#[test]
fn lint_defaults_to_the_workflow_push_branch() {
    let fx = Fixture::new();
    let dest = master_headed_dest(&fx, "dest-master");
    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The 10-13 state: HEAD names an unborn master while the workflow pushes main.
    git_in(
        &fx.home,
        &dest,
        &["symbolic-ref", "HEAD", "refs/heads/master"],
    );
    assert!(git_head(&fx.home, &dest).is_none());

    let output = fx.cmd().args(["lint", "dest-master"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("refs/heads/main"), "{stdout}");

    let elsewhere = fx.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let output = Command::new(bin())
        .env("HOME", &fx.home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(&elsewhere)
        .args([
            "lint",
            dest.to_str().unwrap(),
            "--workflow",
            fx.root.join("workflow.toml").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("refs/heads/main"), "{stdout}");
}

#[test]
fn migrate_strips_a_reference_with_its_punctuation() {
    let fx = Fixture::new();
    write(
        &fx.origin,
        "notes.md",
        "<!-- docs: verified=2026-10-11; owner=PROJ-c0ffee -->\n\
         the parser (PROJ-c0ffee, seen on v2.1) rests\n\
         \x20 PROJ-facade; it replaced the old one\n\
         round 1 (PROJ-decade): a new window\n\
         round 2 (beta, PROJ-bead01) adds two\n",
    );
    git_in(&fx.home, &fx.origin, &["add", "notes.md"]);
    git_in(
        &fx.home,
        &fx.origin,
        &["commit", "-m", "notes (PROJ-abc123)"],
    );

    let output = fx
        .cmd()
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr={stderr}\nstdout={stdout}");

    let notes = git_out(&fx.home, &fx.dest, &["show", "main:notes.md"]);
    assert_eq!(
        notes,
        "<!-- docs: verified=2026-10-11 -->\n\
         the parser (seen on v2.1) rests\n\
         \x20 it replaced the old one\n\
         round 1: a new window\n\
         round 2 (beta) adds two"
    );
    let body = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B", "main"]);
    assert!(!body.contains("()"), "{body}");
    assert!(body.contains("notes\n"), "{body}");
}

// No audio in a public destination, whatever the workflow excludes.
#[test]
fn lint_refuses_binary_audio() {
    let fx = Fixture::new();
    let cases: [(&str, &[u8]); 4] = [
        (
            "voice/a.wav",
            b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00",
        ),
        (
            "voice/b.dat",
            b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00",
        ),
        ("voice/c.bin", b"fLaC\x00\x00\x00\x22\x10\x00\x10\x00"),
        ("voice/d.mp3", b"ID3\x04\x00\x00\x00\x00\x00\x0a"),
    ];
    for (index, (path, bytes)) in cases.iter().enumerate() {
        let name = format!("audio-dest-{index}");
        let repo = fx.root.join(&name);
        git(
            &fx.home,
            None,
            &["init", "-b", "main", repo.to_str().unwrap()],
        );
        let file = repo.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, bytes).unwrap();
        git_in(&fx.home, &repo, &["add", path]);
        git_in(&fx.home, &repo, &["commit", "-m", "voice"]);
        let output = fx.cmd().args(["lint", &name]).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{path} passed the lint");
        assert!(stderr.contains("binary audio"), "{path}: {stderr}");
        assert!(stderr.contains(path), "{path}: {stderr}");
    }

    // A text file that names a sound is not audio.
    let repo = fx.root.join("text-dest");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", repo.to_str().unwrap()],
    );
    write(
        &repo,
        "notes.md",
        "RIFF and WAVE are words; see chime.wav\n",
    );
    git_in(&fx.home, &repo, &["add", "notes.md"]);
    git_in(&fx.home, &repo, &["commit", "-m", "notes"]);
    let output = fx.cmd().args(["lint", "text-dest"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---- the leak lint, `copybera status`, drops and renames ----

/// Runs copybera in `cwd`: (success, stdout, stderr).
fn run_in(fx: &Fixture, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(bin())
        .env("HOME", &fx.home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn commit_file(fx: &Fixture, rel: &str, body: &str, message: &str) {
    write(&fx.origin, rel, body);
    git_in(&fx.home, &fx.origin, &["add", rel]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", message]);
}

// The lint never walks HEAD.
#[test]
fn lint_never_walks_head() {
    let fx = Fixture::new();
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    let elsewhere = fx.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let dest = fx.dest.to_str().unwrap();

    let (ok, stdout, stderr) = run_in(&fx, &elsewhere, &["lint", dest]);
    assert!(!ok, "lint walked HEAD with no workflow: {stdout}");
    assert!(stderr.contains("no branch to walk"), "{stderr}");

    let (ok, stdout, stderr) = run_in(&fx, &elsewhere, &["lint", dest, "--range", "main"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("(main, "), "{stdout}");
}

#[test]
fn lint_refuses_a_private_term_and_never_prints_it() {
    let fx = Fixture::new();
    commit_file(
        &fx,
        "docs/hosts.md",
        "intro\nthe scorer runs on hostname-zebra7 tonight\n",
        "hosts",
    );
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    write(
        &fx.root,
        "private.txt",
        "# hosts, one regex per line\n\nhostname-zebra\\d\n",
    );

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["lint", "dest", "--private", "private.txt"]);
    assert!(!ok, "{stdout}");
    assert!(stderr.contains("private term (pattern 1 of "), "{stderr}");
    assert!(stderr.contains("docs/hosts.md line 2"), "{stderr}");
    assert!(
        !stderr.contains("zebra"),
        "the finding printed the term: {stderr}"
    );

    // fail closed: a named file that is missing, or a pattern that does not compile, refuses the lint
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["lint", "dest", "--private", "absent.txt"]);
    assert!(!ok);
    assert!(stderr.contains("cannot read private patterns"), "{stderr}");
    write(&fx.root, "bad.txt", "ok-word\n(\n");
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["lint", "dest", "--private", "bad.txt"]);
    assert!(!ok);
    assert!(stderr.contains("bad.txt line 2: invalid regex"), "{stderr}");

    // a clean destination passes and says how many patterns it read
    write(&fx.root, "clean.txt", "nothing-like-this\nnor-this\n");
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["lint", "dest", "--private", "clean.txt"]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("2 private patterns"), "{stdout}");
}

#[test]
fn lint_refuses_a_private_term_in_an_exported_filename() {
    let fx = Fixture::new();
    commit_file(
        &fx,
        "docs/hostname-zebra7.md",
        "this file has no private contents\n",
        "private filename",
    );
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    write(&fx.root, "private.txt", "hostname-zebra\\d\n");

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["lint", "dest", "--private", "private.txt"]);
    assert!(!ok, "{stdout}");
    assert!(stderr.contains("private term (pattern 1 of "), "{stderr}");
    assert!(
        stderr.contains("filename <private filename> line 1"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("zebra"),
        "the finding printed the term: {stderr}"
    );
}

#[test]
fn the_workflows_private_patterns_reach_lint_doctor_and_the_receipt() {
    let fx = Fixture::new();
    write(&fx.root, "private.txt", "hostname-zebra\\d\n");
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        format!("{text}\n[lint]\nprivate = \"private.txt\"\n"),
    )
    .unwrap();
    commit_file(&fx, "docs/hosts.md", "on hostname-zebra7\n", "hosts");

    // migrate writes; the lint is the gate
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    let named = receipt["private_patterns"].as_str().unwrap_or_default();
    assert!(named.ends_with("private.txt"), "{receipt}");

    let (ok, _, stderr) = run_in(&fx, &fx.root, &["lint", "dest"]);
    assert!(!ok);
    assert!(stderr.contains("private term"), "{stderr}");
    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &["lint", "--from-receipt", "fixture.receipt.json"],
    );
    assert!(!ok);
    assert!(stderr.contains("private term"), "{stderr}");

    let (_, stdout, _) = run_in(&fx, &fx.root, &["doctor", "workflow.toml"]);
    assert!(
        stdout.contains("ok private patterns: 1 pattern"),
        "{stdout}"
    );
    fs::remove_file(fx.root.join("private.txt")).unwrap();
    let (ok, stdout, _) = run_in(&fx, &fx.root, &["doctor", "workflow.toml"]);
    assert!(!ok);
    assert!(stdout.contains("FAIL private patterns"), "{stdout}");
}

#[test]
fn private_scrub_rules_apply_after_workflow_transformations_and_keep_rule_text_private() {
    let fx = Fixture::new();
    write(
        &fx.root,
        "private-scrub.txt",
        "# regex<TAB>replacement\nhostname-zebra\\d\t<private host>\nsecret-owner\tthe maintainer\n",
    );
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        format!("{text}\n[scrub]\nprivate = \"private-scrub.txt\"\n"),
    )
    .unwrap();
    commit_file(
        &fx,
        "docs/private.md",
        "runs on hostname-zebra7 for secret-owner\n",
        "hostname-zebra7: secret-owner",
    );

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("scrub.private"), "{stdout}");
    assert!(stdout.contains("rule 1"), "{stdout}");
    assert!(!stdout.contains("zebra"), "{stdout}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert!(receipt["private_scrub"]
        .as_str()
        .unwrap()
        .ends_with("private-scrub.txt"));
    assert_eq!(receipt["private_scrub_rule_count"], 2, "{receipt}");
    let exported = git_out(&fx.home, &fx.dest, &["show", "main:docs/private.md"]);
    assert_eq!(exported, "runs on <private host> for the maintainer");
    let subject = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%s", "main"]);
    assert!(
        !subject.contains("zebra") && !subject.contains("secret-owner"),
        "{subject}"
    );

    write(&fx.root, "bad-scrub.txt", "ok\tvalue\nmissing-tab\n");
    let bad = fs::read_to_string(&workflow)
        .unwrap()
        .replace("private-scrub.txt", "bad-scrub.txt");
    fs::write(&workflow, bad).unwrap();
    let (ok, stdout, _) = run_in(&fx, &fx.root, &["doctor", "workflow.toml"]);
    assert!(!ok);
    assert!(stdout.contains("rule 2"), "{stdout}");
    assert!(!stdout.contains("missing-tab"), "{stdout}");
}

#[test]
fn private_scrub_rules_keep_replacement_spaces_and_allow_empty_replacements() {
    let fx = Fixture::new();
    write(
        &fx.root,
        "private-scrub.txt",
        "delete-me\t\nsecret-owner\t  the maintainer  \n",
    );
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        format!("{text}\n[scrub]\nprivate = \"private-scrub.txt\"\n"),
    )
    .unwrap();
    commit_file(
        &fx,
        "docs/private.md",
        "gone delete-me; owner secret-owner now\n",
        "private scrub",
    );

    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    let exported = git_out(&fx.home, &fx.dest, &["show", "main:docs/private.md"]);
    assert_eq!(exported, "gone ; owner   the maintainer   now");
}

#[test]
fn lint_allows_named_private_patterns_only_in_named_paths() {
    let fx = Fixture::new();
    write(&fx.root, "private.txt", "hostname-zebra\\d\n");
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        format!(
            "{text}\n[lint]\nprivate = \"private.txt\"\n\n[[lint.allow]]\npaths = [\"docs/allowed/**\"]\npatterns = [1]\n"
        ),
    )
    .unwrap();
    commit_file(
        &fx,
        "docs/allowed/hostname-zebra7.md",
        "this file has no private contents\n",
        "allowed host",
    );

    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["lint", "dest"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &["lint", "--from-receipt", "fixture.receipt.json"],
    );
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let (_, doctor, _) = run_in(&fx, &fx.root, &["doctor", "workflow.toml"]);
    assert!(
        doctor.contains("ok lint allowances: 1 allowance"),
        "{doctor}"
    );

    commit_file(
        &fx,
        "docs/denied/hostname-zebra8.md",
        "this file has no private contents\n",
        "denied host",
    );
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["lint", "dest"]);
    assert!(!ok);
    assert!(stderr.contains("pattern 1"), "{stderr}");
    assert!(
        stderr.contains("filename <private filename> line 1"),
        "{stderr}"
    );
    assert!(!stderr.contains("zebra"), "{stderr}");
}

#[test]
fn status_reads_every_workflows_receipt_into_one_table() {
    let fx = Fixture::new();
    let ids = origin_ids(&fx);
    // a second workflow beside the first, never run
    let second = fx.root.join("second.toml");
    write_workflow(&second, "squash", "dest-second");
    let text = fs::read_to_string(&second)
        .unwrap()
        .replace("name = \"fixture\"", "name = \"second\"")
        .replace("fixture.receipt.json", "second.receipt.json");
    fs::write(&second, text).unwrap();

    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &["migrate", "workflow.toml", "--max-commits", "2"],
    );
    assert!(ok, "{stderr}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert!(receipt["seconds"].as_f64().is_some(), "{receipt}");
    // The receipt says the subjects are the fallback's (the model hook is reserved, not wired).
    assert_eq!(receipt["model_hook"], "stubbed", "{receipt}");

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["status", "."]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    for column in [
        "workflow", "last_rev", "bounded", "commits", "seconds", "subjects", "doctor",
    ] {
        assert!(lines[0].contains(column), "header lacks {column}: {stdout}");
    }
    let first = lines
        .iter()
        .find(|line| line.starts_with("fixture "))
        .unwrap_or_else(|| panic!("no fixture row: {stdout}"));
    assert!(first.contains(&ids[1][..10]), "{first}");
    assert!(first.contains("true"), "{first}");
    assert!(first.contains("2/1"), "{first}");
    assert!(first.contains("fallback"), "{first}");
    assert!(first.trim_end().ends_with("ok"), "{first}");
    let second = lines
        .iter()
        .find(|line| line.starts_with("second "))
        .unwrap_or_else(|| panic!("no second row: {stdout}"));
    assert!(second.contains("no receipt"), "{second}");
    assert!(second.contains("FAIL destination bare"), "{second}");

    // named files work too, and a missing directory is refused
    let (ok, stdout, _) = run_in(&fx, &fx.root, &["status", "workflow.toml"]);
    assert!(ok);
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["status", "absent"]);
    assert!(!ok);
    assert!(stderr.contains("absent"), "{stderr}");
}

// ---- scheduled migrate and watched status ----

#[test]
fn scheduled_migrate_plan_is_read_only_and_refuses_short_or_invalid_intervals() {
    let fx = Fixture::new();
    let receipt = fx.root.join("fixture.receipt.json");

    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "migrate",
            "--schedule",
            "workflow.toml",
            "--every",
            "60s",
            "--plan",
        ],
    );
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    assert_eq!(stdout.lines().count(), 3, "{stdout}");
    for line in stdout.lines() {
        assert!(line.contains("UTC"), "{line}");
        assert!(line.contains("copybera migrate workflow.toml"), "{line}");
    }
    assert!(!receipt.exists(), "plan wrote a receipt");

    for every in ["0s", "59s", "nope", "1d"] {
        let (ok, _, stderr) = run_in(
            &fx,
            &fx.root,
            &[
                "migrate",
                "--schedule",
                "workflow.toml",
                "--every",
                every,
                "--plan",
            ],
        );
        assert!(!ok, "{every} was accepted");
        assert!(stderr.contains("duration"), "{stderr}");
    }
}

#[test]
fn scheduled_migrate_once_writes_a_receipt() {
    let fx = Fixture::new();
    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "migrate",
            "--schedule",
            "workflow.toml",
            "--every",
            "60s",
            "--once",
        ],
    );
    assert!(ok, "{stderr}");
    assert!(fx.root.join("fixture.receipt.json").is_file());
}

#[test]
fn scheduled_resume_uses_a_bounded_receipt_then_the_destination_revision() {
    let fx = Fixture::new();
    let ids = origin_ids(&fx);
    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "migrate",
            "--schedule",
            "workflow.toml",
            "--every",
            "60s",
            "--once",
            "--max-commits",
            "2",
            "--resume",
        ],
    );
    assert!(ok, "{stderr}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert_eq!(receipt["last_rev"], ids[1]);
    assert_eq!(receipt["bounded"], true);

    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "migrate",
            "--schedule",
            "workflow.toml",
            "--every",
            "60s",
            "--once",
            "--resume",
        ],
    );
    assert!(ok, "{stderr}");
    // The resumed cycle walks only the tracker-only commit, which changes no kept path: it is dropped, as a whole
    // walk drops it, and no empty commit is written. The destination stays at the bounded cycle's
    // revision.
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert_eq!(receipt["origin_range"]["resolved_since"], ids[1]);
    assert_eq!(drop_ids(&receipt), vec![ids[2].clone()], "{receipt}");
    assert!(git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B"])
        .contains(&format!("Copybera-RevId: {}", ids[1])));

    // The next cycle walks on from the destination's revision and writes the kept change.
    commit_file(&fx, "keep.txt", "hello again\n", "a kept change");
    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "migrate",
            "--schedule",
            "workflow.toml",
            "--every",
            "60s",
            "--once",
            "--resume",
        ],
    );
    assert!(ok, "{stderr}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert_eq!(receipt["origin_range"]["resolved_since"], ids[1]);
    let origin_head = git_out(&fx.home, &fx.origin, &["rev-parse", "HEAD"]);
    assert!(git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B"])
        .contains(&format!("Copybera-RevId: {origin_head}")));

    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "migrate",
            "--schedule",
            "workflow.toml",
            "--every",
            "60s",
            "--once",
            "--resume",
        ],
    );
    assert!(ok, "{stderr}");
    assert!(stdout.contains("up to date"), "{stdout}");
}

/// The first scheduled cycle of a workflow with
/// `origin.since`, into a destination that carries no `Copybera-RevId` yet, walked the whole origin history. With
/// nothing to walk on from, it starts at `origin.since`, as a plain migrate does.
#[test]
fn scheduled_resume_into_an_empty_destination_starts_at_origin_since() {
    let fx = Fixture::new();
    let ids = origin_ids(&fx);
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        text.replacen(
            "ref = \"main\"\n",
            &format!("ref = \"main\"\nsince = \"{}\"\n", ids[0]),
            1,
        ),
    )
    .unwrap();

    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "migrate",
            "--schedule",
            "workflow.toml",
            "--every",
            "60s",
            "--once",
            "--resume",
        ],
    );
    assert!(ok, "{stderr}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert_eq!(receipt["commits_read"], 2, "{receipt}");
    assert_eq!(
        receipt["origin_range"]["resolved_since"],
        ids[0].as_str(),
        "{receipt}"
    );
}

#[cfg(unix)]
#[test]
fn scheduled_migrate_stops_cleanly_after_sigterm() {
    let fx = Fixture::new();
    let mut child = fx
        .cmd()
        .args(["migrate", "--schedule", "workflow.toml", "--every", "60s"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fx.root.join("fixture.receipt.json").is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        fx.root.join("fixture.receipt.json").is_file(),
        "scheduled migration did not finish its first cycle"
    );
    // SAFETY: the child belongs to this test process and is still running its scheduled loop.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(child.wait().unwrap().success());
}

#[test]
fn status_watch_reprints_the_table_for_each_requested_count() {
    let fx = Fixture::new();
    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "status",
            "workflow.toml",
            "--watch",
            "--interval",
            "1s",
            "--count",
            "2",
        ],
    );
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.starts_with("workflow"))
            .count(),
        2,
        "{stdout}"
    );
}

// A commit "empty after strip" leaves its trace in the receipt's drops.
#[test]
fn migrate_receipt_keeps_every_dropped_commit_as_the_trace() {
    let fx = Fixture::new();
    let ids = origin_ids(&fx);
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    let drops = receipt["drops"].as_array().unwrap();
    assert!(!drops.is_empty(), "{receipt}");
    assert_eq!(receipt["commits_dropped"], drops.len() as u64, "{receipt}");
    assert!(
        drops.iter().any(|d| d["commit"] == ids[2].as_str()),
        "the tracker-only commit has no trace: {receipt}"
    );
}

/// Three origin commits where the second changes only a path the workflow strips: (first, tracker only, third).
fn three_commit_origin(fx: &Fixture) -> (PathBuf, Vec<String>) {
    let origin = fx.root.join("origin-three");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", origin.to_str().unwrap()],
    );
    write(&origin, "keep.txt", "one\n");
    write(&origin, ".logbook/item.md", "a\n");
    git_in(&fx.home, &origin, &["add", "keep.txt", ".logbook/item.md"]);
    git_in(&fx.home, &origin, &["commit", "-m", "first"]);
    write(&origin, ".logbook/item.md", "b\n");
    git_in(&fx.home, &origin, &["add", ".logbook/item.md"]);
    git_in(&fx.home, &origin, &["commit", "-m", "tracker only"]);
    write(&origin, "keep.txt", "three\n");
    git_in(&fx.home, &origin, &["add", "keep.txt"]);
    git_in(&fx.home, &origin, &["commit", "-m", "third"]);
    let ids = git_out(
        &fx.home,
        &origin,
        &["rev-list", "--reverse", "--first-parent", "main"],
    )
    .lines()
    .map(str::to_string)
    .collect();
    (origin, ids)
}

/// A workflow over `origin-three` into a fresh bare `dest`, with its own receipt file.
fn three_commit_workflow(fx: &Fixture, name: &str, mode: &str) -> PathBuf {
    let dest = format!("dest-{name}");
    git(
        &fx.home,
        None,
        &[
            "init",
            "--bare",
            "-b",
            "main",
            fx.root.join(&dest).to_str().unwrap(),
        ],
    );
    let file = fx.root.join(format!("{name}.toml"));
    write_workflow(&file, mode, &dest);
    let text = fs::read_to_string(&file)
        .unwrap()
        .replace("path = \"origin\"", "path = \"origin-three\"")
        .replace("fixture.receipt.json", &format!("{name}.receipt.json"));
    fs::write(&file, text).unwrap();
    file
}

fn drop_ids(receipt: &serde_json::Value) -> Vec<String> {
    receipt["drops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["commit"].as_str().unwrap().to_string())
        .collect()
}

// A `--resume` walk once compared its first commit with the RAW tree at
// resolved_since, so a first commit that changes only stripped paths was kept in a resumed run and dropped in a
// whole one. The resumed drops are the whole run's drops, and a per-commit export writes no empty commit at the
// resume point.
#[test]
fn a_resumed_walk_drops_a_first_commit_that_changes_no_kept_path() {
    let fx = Fixture::new();
    let (_, ids) = three_commit_origin(&fx);
    assert_eq!(ids.len(), 3);
    for mode in ["squash", "iterative"] {
        let whole = three_commit_workflow(&fx, &format!("whole-{mode}"), mode);
        let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", whole.to_str().unwrap()]);
        assert!(ok, "{mode} whole: {stderr}");
        let whole_receipt = read_json(&fx.root.join(format!("whole-{mode}.receipt.json")));
        assert_eq!(
            drop_ids(&whole_receipt),
            vec![ids[1].clone()],
            "{whole_receipt}"
        );

        let bounded = three_commit_workflow(&fx, &format!("bounded-{mode}"), mode);
        let receipt_file = fx.root.join(format!("bounded-{mode}.receipt.json"));
        let (ok, _, stderr) = run_in(
            &fx,
            &fx.root,
            &["migrate", bounded.to_str().unwrap(), "--max-commits", "1"],
        );
        assert!(ok, "{mode} bounded: {stderr}");
        let first = read_json(&receipt_file);
        assert_eq!(first["last_rev"], ids[0].as_str(), "{first}");
        assert!(drop_ids(&first).is_empty(), "{first}");

        let (ok, _, stderr) = run_in(
            &fx,
            &fx.root,
            &["migrate", bounded.to_str().unwrap(), "--resume"],
        );
        assert!(ok, "{mode} resume: {stderr}");
        let resumed = read_json(&receipt_file);
        assert_eq!(resumed["origin_range"]["resolved_since"], ids[0].as_str());
        assert_eq!(
            drop_ids(&resumed),
            vec![ids[1].clone()],
            "{mode}: the resumed walk kept the commit the whole run drops: {resumed}"
        );
        assert_eq!(resumed["commits_dropped"], 1, "{resumed}");

        let whole_dest = fx.root.join(format!("dest-whole-{mode}"));
        let bounded_dest = fx.root.join(format!("dest-bounded-{mode}"));
        assert_eq!(
            git_out(&fx.home, &bounded_dest, &["rev-parse", "main^{tree}"]),
            git_out(&fx.home, &whole_dest, &["rev-parse", "main^{tree}"]),
            "{mode}"
        );
        if mode == "iterative" {
            assert_eq!(
                git_out(&fx.home, &bounded_dest, &["rev-list", "--count", "main"]),
                git_out(&fx.home, &whole_dest, &["rev-list", "--count", "main"]),
                "a per-commit export wrote an empty commit at the resume point"
            );
        }
    }
}

// A bounded squash carries the file as it was at the last walked commit, before a later rename landed; no
// transformation rewrites a path the workflow does not name.
#[test]
fn a_bounded_squash_carries_the_last_walked_tree_not_a_rewrite() {
    let fx = Fixture::new();
    commit_file(&fx, "apps/README.md", "the agent: agents/alpha\n", "readme");
    commit_file(
        &fx,
        "apps/README.md",
        "the agent: catalog/agents/beta\n",
        "rename the agent",
    );
    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &["migrate", "workflow.toml", "--max-commits", "4"],
    );
    assert!(ok, "{stderr}");
    assert_eq!(
        git_out(&fx.home, &fx.dest, &["show", "main:apps/README.md"]),
        "the agent: agents/alpha"
    );
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml", "--resume"]);
    assert!(ok, "{stderr}");
    assert_eq!(
        git_out(&fx.home, &fx.dest, &["show", "main:apps/README.md"]),
        "the agent: catalog/agents/beta"
    );
}

// A public destination never carries the local git identity: the committer is the workflow's authoring
// default, and the leak lint reads every commit's author and committer too.
#[test]
fn migrate_commits_as_the_authoring_default_and_the_leak_lint_reads_identities() {
    let fx = Fixture::new();
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    assert_eq!(
        git_out(
            &fx.home,
            &fx.dest,
            &["log", "-1", "--format=%cn <%ce>", "main"]
        ),
        "Fixture Authors <authors@example.invalid>"
    );

    let repo = fx.root.join("identity-dest");
    git(
        &fx.home,
        None,
        &["init", "-b", "main", repo.to_str().unwrap()],
    );
    write(&repo, "notes.md", "plain text\n");
    git_in(&fx.home, &repo, &["add", "notes.md"]);
    git_in_as(
        &fx.home,
        &repo,
        "hostname-zebra7",
        "someone@example.invalid",
        &["commit", "-m", "notes"],
    );
    write(&fx.root, "private.txt", "hostname-zebra\\d\n");
    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &["lint", "identity-dest", "--private", "private.txt"],
    );
    assert!(!ok, "{stdout}");
    assert!(stderr.contains("private term (pattern 1 of "), "{stderr}");
    assert!(stderr.contains("<identity>"), "{stderr}");
    assert!(!stderr.contains("zebra"), "{stderr}");
}

// Git quotes a path with a non-ASCII byte, a quote, a
// backslash or a tab in `ls-tree` output unless `-z` is given, and the export wrote the quoted form as a new path
// (two fixture files under `remote/é@…/` came out as `"remote/\303\251@…"`).
#[test]
fn migrate_exports_unusual_path_names_verbatim() {
    let fx = Fixture::new();
    let names = [
        "remote/é@192.0.2.4/stdout",
        "quote\"d.txt",
        "back\\slash.txt",
        "tab\tname.txt",
        "naïve dir/file name.md",
    ];
    for name in names {
        write(&fx.origin, name, "x\n");
        git_in(&fx.home, &fx.origin, &["add", "--", name]);
    }
    git_in(&fx.home, &fx.origin, &["commit", "-m", "unusual names"]);

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &[
            "-c",
            "core.quotePath=false",
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            "main",
        ],
    );
    let exported: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    for name in names {
        assert!(
            exported.contains(&name),
            "{name:?} missing from {exported:?}"
        );
    }
    assert!(
        !exported.iter().any(|p| p.starts_with('"')),
        "a quoted path was exported: {exported:?}"
    );
}

// Every file keeps its git mode: an export that wrote every file as 100644 lost executables and folder symlinks (a
// symlink came out as a text file holding its target). Modes follow a moved path.
#[test]
fn migrate_keeps_executable_bits_and_symlinks() {
    let fx = Fixture::new();
    write(&fx.origin, "tools/run.sh", "#!/bin/sh\necho ok\n");
    let script = fx.origin.join("tools/run.sh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("src", fx.origin.join("src-link")).unwrap();
    git_in(&fx.home, &fx.origin, &["add", "tools/run.sh", "src-link"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "script and link"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!(
            "{workflow}\n[[transformations]]\ntype = \"rename_paths\"\nfrom = \"tools/**\"\nto = \"bin/\"\n"
        ),
    )
    .unwrap();

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let listing = git_out(&fx.home, &fx.dest, &["ls-tree", "-r", "main"]);
    let mode = |path: &str| {
        listing
            .lines()
            .find(|line| line.ends_with(&format!("\t{path}")))
            .and_then(|line| line.split_whitespace().next())
            .map(str::to_string)
    };
    assert_eq!(mode("bin/run.sh").as_deref(), Some("100755"), "{listing}");
    assert_eq!(mode("src-link").as_deref(), Some("120000"), "{listing}");
    assert_eq!(mode("keep.txt").as_deref(), Some("100644"), "{listing}");
    assert_eq!(
        git_out(&fx.home, &fx.dest, &["cat-file", "-p", "main:src-link"]),
        "src"
    );
}

// `copybera gate <workflow>`: the
// destination's push-branch tree scanned by class, a table of classes and counts, never the matched text; a LAN
// address is a finding only inside a network the workflow names (else a counted `lan-example`); `[[gate.allow]]`
// exempts a class under path globs; any finding refuses.
#[test]
fn gate_counts_classes_in_the_exported_tree_and_never_prints_a_match() {
    let fx = Fixture::new();
    write(
        &fx.origin,
        "docs/leaky.md",
        "cd /Users/alice/src\nsee /Volumes/Archive/x and /Users/you/ok\nmail bob@corp.io or a@example.invalid\n\
         host 10.1.0.5, example 10.9.9.9\nentity person.alice\nthe secretword\nold oldname name\n",
    );
    // Built from two halves so the source holds no key-shaped literal for a secret scanner to flag.
    write(
        &fx.origin,
        "tests/corpus.txt",
        concat!("AKIA", "QWERTYUIOPASDFGH", "\n"),
    );
    git_in(
        &fx.home,
        &fx.origin,
        &["add", "docs/leaky.md", "tests/corpus.txt"],
    );
    git_in(&fx.home, &fx.origin, &["commit", "-m", "leaky"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!(
            "{workflow}\n[gate]\nnetworks = [\"10.1.0.0/24\"]\nplaceholders = [\"you\"]\nleftover = ['(?i)oldname']\n\
             email_allow = ['@example\\.invalid$']\n"
        ),
    )
    .unwrap();
    fs::write(fx.root.join("private.txt"), "secretword\n").unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");

    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &["gate", "workflow.toml", "--private", "private.txt"],
    );
    assert!(!ok, "a finding refuses\nstdout={stdout}\nstderr={stderr}");
    let row = |class: &str| {
        stdout
            .lines()
            .find(|line| line.split_whitespace().next() == Some(class))
            .map(|line| {
                line.split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    };
    for (class, counts) in [
        ("home-path", "1 1"),
        ("volume-path", "1 1"),
        ("email", "1 1"),
        ("lan-address", "1 1"),
        ("lan-example", "1 1"),
        ("token", "1 1"),
        ("person-entity", "1 1"),
        ("private-term", "1 1"),
        ("leftover", "1 1"),
    ] {
        assert_eq!(row(class).as_deref(), Some(counts), "{class}\n{stdout}");
    }
    assert!(stdout.contains("findings: 8"), "{stdout}");
    for secret in [
        "alice",
        "Archive",
        "bob@corp",
        "secretword",
        "10.1.0.5",
        "AKIA",
    ] {
        assert!(!stdout.contains(secret), "{secret} printed\n{stdout}");
        assert!(!stderr.contains(secret), "{secret} printed\n{stderr}");
    }

    // An allowance exempts one class under its paths; the rest still refuse.
    let workflow = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!(
            "{workflow}\n[[gate.allow]]\nclass = \"token\"\npaths = [\"tests/**\"]\n\
             reason = \"the detector's synthetic corpus\"\n"
        ),
    )
    .unwrap();
    let (ok, stdout, _) = run_in(
        &fx,
        &fx.root,
        &["gate", "workflow.toml", "--private", "private.txt"],
    );
    assert!(!ok);
    assert_eq!(row_of(&stdout, "token").as_deref(), Some("0 0"), "{stdout}");
    assert!(stdout.contains("findings: 7"), "{stdout}");
}

fn row_of(stdout: &str, class: &str) -> Option<String> {
    stdout
        .lines()
        .find(|line| line.split_whitespace().next() == Some(class))
        .map(|line| {
            line.split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ")
        })
}

// The receipt counts what each rule did: one event count per transformation label (paths stripped per rule, paths
// moved) and the move totals (moves left in place).
#[test]
fn the_receipt_counts_events_per_transformation_and_moves() {
    let fx = Fixture::new();
    write(&fx.origin, "apps/acme-admin/main.rs", "fn main() {}\n");
    write(&fx.origin, "fixtures/acme.json", "{}\n");
    write(&fx.origin, "fixtures/rebrand.json", "{}\n");
    git_in(
        &fx.home,
        &fx.origin,
        &[
            "add",
            "apps/acme-admin/main.rs",
            "fixtures/acme.json",
            "fixtures/rebrand.json",
        ],
    );
    git_in(&fx.home, &fx.origin, &["commit", "-m", "named paths"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!("{workflow}\n[[transformations]]\ntype = \"move_paths\"\npairs = [[\"acme\", \"rebrand\"]]\n"),
    )
    .unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert_eq!(receipt["paths_moved"], 1, "{receipt}");
    assert_eq!(receipt["paths_stayed"], 1, "{receipt}");
    let events = receipt["transform_events"]
        .as_object()
        .expect("transform_events");
    let moves = events
        .iter()
        .find(|(label, _)| label.ends_with(".move_paths"))
        .map(|(_, count)| count.clone());
    assert_eq!(moves, Some(serde_json::json!(2)), "{receipt}");
    // The origin path filter runs as the trees load, so its strips are not events; a strip_paths rule's are.
    assert!(
        events["transformations[0].strip_paths"].as_u64().unwrap() >= 1,
        "{receipt}"
    );
}

// The export must carry no private term: strip_matching `private = true` strips a file
// inside its paths that matches a pattern from the workflow's private-patterns file (outside git), so a tracked
// workflow strips flagged receipts without naming a term. Events name the pattern number only.
#[test]
fn strip_matching_reads_the_private_patterns_file() {
    let fx = Fixture::new();
    write(&fx.origin, "results/a.md", "ran with Zorblax today\n");
    write(&fx.origin, "results/b.md", "clean\n");
    write(&fx.origin, "docs/c.md", "Zorblax in prose\n");
    git_in(&fx.home, &fx.origin, &["add", "results", "docs"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "receipts"]);
    fs::write(fx.root.join("private.txt"), "# private\n(?i)zorblax\n").unwrap();
    let workflow = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!(
            "{workflow}\n[[transformations]]\ntype = \"strip_matching\"\npaths = [\"results/**\"]\nprivate = true\n\n\
             [lint]\nprivate = \"private.txt\"\n"
        ),
    )
    .unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "main"],
    );
    assert!(!listing.contains("results/a.md"), "{listing}");
    assert!(listing.contains("results/b.md"), "{listing}");
    assert!(listing.contains("docs/c.md"), "outside paths\n{listing}");
    assert!(
        stdout.contains("path dropped (private pattern 1)"),
        "{stdout}"
    );
    assert!(!stdout.to_lowercase().contains("zorblax"), "{stdout}");
}

// The licence and notice classes: an SPDX header off the
// redistribution list, a licence file whose text is a Commons Clause, a carried upstream (the notices manifest) under a
// licence off the list, a required notice missing, and a licence or notice file the export dropped while files beside
// it were exported (read against the origin's paths).
#[test]
fn gate_refuses_licences_off_the_redistribution_list_and_dropped_notices() {
    let fx = Fixture::new();
    write(
        &fx.origin,
        "LICENSE",
        "MIT License\n\nPermission is hereby granted, free of charge\n",
    );
    write(
        &fx.origin,
        "vendor/bits/LICENSE",
        "\"Commons Clause\" License Condition v1.0\n\nMIT License\n\nPermission is hereby granted, free of charge\n",
    );
    write(
        &fx.origin,
        "vendor/bits/bits.js",
        "export const bits = 1;\n",
    );
    write(
        &fx.origin,
        "src/gpl.rs",
        "// SPDX-License-Identifier: GPL-3.0-only\npub fn gpl() {}\n",
    );
    write(
        &fx.origin,
        "src/ok.rs",
        "// SPDX-License-Identifier: MIT OR Apache-2.0\npub fn ok() {}\n",
    );
    write(
        &fx.origin,
        "tools/notices.toml",
        "[[source]]\nkind = \"port\"\nlicence = \"MIT AND Commons Clause\"\npaths = [\"vendor/port\"]\n\n\
         [[source]]\nkind = \"port\"\nlicence = \"MIT\"\npaths = [\"vendor/okport\"]\n\n\
         [[source]]\nkind = \"inspiration\"\nlicence = \"FSL-1.1\"\npaths = [\"src\"]\n",
    );
    write(&fx.origin, "vendor/port/port.ts", "export {};\n");
    write(&fx.origin, "vendor/okport/okport.ts", "export {};\n");
    write(&fx.origin, "third/NOTICE", "Third upstream notice\n");
    write(&fx.origin, "third/third.txt", "third\n");
    git_in(&fx.home, &fx.origin, &["add", "."]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "licences"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml"))
        .unwrap()
        .replace(
            "exclude = [\"secret/**\"]",
            "exclude = [\"secret/**\", \"third/NOTICE\"]",
        );
    fs::write(
        fx.root.join("workflow.toml"),
        format!("{workflow}\n[gate]\n[gate.licence]\ncarried = \"tools/notices.toml\"\n"),
    )
    .unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["gate", "workflow.toml", "--files"]);
    assert!(
        !ok,
        "a licence finding refuses\nstdout={stdout}\nstderr={stderr}"
    );
    assert_eq!(
        row_of(&stdout, "licence").as_deref(),
        Some("3 3"),
        "{stdout}"
    );
    assert_eq!(
        row_of(&stdout, "notice").as_deref(),
        Some("2 2"),
        "{stdout}"
    );
    for row in [
        "licence 1 vendor/bits/LICENSE",
        "licence 1 src/gpl.rs",
        "licence 1 vendor/port",
        "notice 1 THIRD_PARTY_NOTICES.md",
        "notice 1 third/NOTICE",
    ] {
        assert!(stdout.lines().any(|line| line == row), "{row}\n{stdout}");
    }
    assert!(stdout.contains("findings: 5"), "{stdout}");
    assert!(
        stdout.contains("licences: 2 SPDX headers, 1 licence files, 2 carried upstreams, dropped notices checked"),
        "{stdout}"
    );

    // Fixed upstreams, the notice carried and the dropped notice exported: clean.
    write(
        &fx.origin,
        "vendor/bits/LICENSE",
        "MIT License\n\nPermission is hereby granted, free of charge\n",
    );
    write(
        &fx.origin,
        "src/gpl.rs",
        "// SPDX-License-Identifier: MIT\npub fn gpl() {}\n",
    );
    write(
        &fx.origin,
        "tools/notices.toml",
        "[[source]]\nkind = \"port\"\nlicence = \"MIT\"\npaths = [\"vendor/port\"]\n",
    );
    write(&fx.origin, "THIRD_PARTY_NOTICES.md", "# Notices\n");
    git_in(&fx.home, &fx.origin, &["add", "."]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "fixed licences"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml"))
        .unwrap()
        .replace(", \"third/NOTICE\"]", "]");
    fs::write(fx.root.join("workflow.toml"), workflow).unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["gate", "workflow.toml"]);
    assert!(ok, "stdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains("findings: 0"), "{stdout}");
}

// A tracked gate file (`--config`) replaces a generated workflow's `[gate]`: legacy-name families with
// lookaround claim their spans in order after the kept names, only under `leftover_paths` and outside the alias lines;
// a path named for an old name is one `leftover-path`; a private path is refused unread; audio is refused by signature;
// `--files` prints class, count and path, never the text.
#[test]
fn gate_config_file_judges_legacy_names_private_paths_and_audio() {
    let fx = Fixture::new();
    write(
        &fx.origin,
        "src/a.rs",
        "let x = OldnameConfig::new();\nlet alias = \"rebrand OldnameConfig\";\nsign ~/.config/acme/codesign\n\
         cache ~/.config/acme/cache\n",
    );
    write(&fx.origin, "docs/prose.md", "the oldname days\n");
    write(&fx.origin, "notes/data.csv", "OldnameConfig\n");
    write(&fx.origin, "apps/acme-old/readme.txt", "old app\n");
    write(&fx.origin, ".claude/notes.md", "session\n");
    let mut wav = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
    wav.extend_from_slice(&[0u8; 32]);
    fs::create_dir_all(fx.origin.join("assets")).unwrap();
    fs::write(fx.origin.join("assets/clip.bin"), wav).unwrap();
    git_in(&fx.home, &fx.origin, &["add", "."]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "legacy"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!("{workflow}\n[gate]\nleftover = ['nothing-here']\n"),
    )
    .unwrap();
    fs::create_dir_all(fx.root.join("gates")).unwrap();
    fs::write(
        fx.root.join("gates/public.toml"),
        r#"[gate]
leftover = [
  '(?<![A-Za-z0-9_])(?:[A-Za-z0-9_]+(?:oldname|Oldname|OLDNAME)[A-Za-z0-9_]*|(?:oldname|Oldname|OLDNAME)[A-Za-z0-9_]+)',
  '\.(?:config|local/state|local/share|cache)/acme[A-Za-z0-9_-]*',
]
leftover_keep = ['\.config/acme/codesign']
leftover_paths = ["**/*.rs", "**/*.md"]
leftover_lines = ['(?i)oldname|acme']
leftover_ignore_lines = ['(?i)rebrand']
leftover_path = ['(?i)oldname|acme']
private_paths = [".claude/**"]
"#,
    )
    .unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");

    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "gate",
            "workflow.toml",
            "--config",
            "gates/public.toml",
            "--files",
        ],
    );
    assert!(!ok, "stdout={stdout}\nstderr={stderr}");
    for (class, counts) in [
        ("leftover", "2 1"),
        ("leftover-path", "1 1"),
        ("private-path", "1 1"),
        ("audio", "1 1"),
    ] {
        assert_eq!(
            row_of(&stdout, class).as_deref(),
            Some(counts),
            "{class}\n{stdout}"
        );
    }
    for row in [
        "leftover 2 src/a.rs",
        "leftover-path 1 apps/acme-old/readme.txt",
        "private-path 1 .claude/notes.md",
        "audio 1 assets/clip.bin",
    ] {
        assert!(stdout.lines().any(|line| line == row), "{row}\n{stdout}");
    }
    assert!(stdout.contains("findings: 5"), "{stdout}");
    assert!(!stdout.contains("OldnameConfig"), "{stdout}");

    // An allowance names its reason; a private term is never allowed.
    let gate = fs::read_to_string(fx.root.join("gates/public.toml")).unwrap();
    fs::write(
        fx.root.join("gates/public.toml"),
        format!("{gate}\n[[gate.allow]]\nclass = \"audio\"\npaths = [\"assets/**\"]\n"),
    )
    .unwrap();
    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &["gate", "workflow.toml", "--config", "gates/public.toml"],
    );
    assert!(
        !ok && stderr.contains("gate.allow[0].reason: is required"),
        "{stderr}"
    );
    fs::write(
        fx.root.join("gates/public.toml"),
        format!(
            "{gate}\n[[gate.allow]]\nclass = \"private-term\"\npaths = [\"**\"]\nreason = \"no\"\n"
        ),
    )
    .unwrap();
    let (ok, _, stderr) = run_in(
        &fx,
        &fx.root,
        &["gate", "workflow.toml", "--config", "gates/public.toml"],
    );
    assert!(!ok && stderr.contains("never allowed"), "{stderr}");
    fs::write(
        fx.root.join("gates/public.toml"),
        format!(
            "{gate}\n[[gate.allow]]\nclass = \"audio\"\npaths = [\"assets/**\"]\nreason = \"a synthetic header\"\n"
        ),
    )
    .unwrap();
    let (ok, stdout, _) = run_in(
        &fx,
        &fx.root,
        &["gate", "workflow.toml", "--config", "gates/public.toml"],
    );
    assert!(!ok);
    assert_eq!(row_of(&stdout, "audio").as_deref(), Some("0 0"), "{stdout}");
    assert!(stdout.contains("findings: 4"), "{stdout}");
}

// An export that keeps reference ids on purpose (strip_references off) names them in `[lint] ids` and is still linted
// for everything else: `--keep-references` or `[lint] references = "keep"` stops refusing them in messages and file
// contents, while a path that matches one still refuses; `--count` walks everything and prints one row per check (the
// fixture's own commit message keeps its id too).
#[test]
fn lint_counts_every_check_and_keeps_references_on_request() {
    let fx = Fixture::new();
    write(
        &fx.origin,
        "docs/ids.md",
        "see PROJ-abc123 and PROJ-def456 in .logbook\n",
    );
    git_in(&fx.home, &fx.origin, &["add", "docs/ids.md"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "ids"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml"))
        .unwrap()
        .replace(
            "[[transformations]]\ntype = \"strip_references\"\npatterns = [\"PROJ-[0-9a-f]{6}\", \"logbook\"]\n",
            "",
        )
        .replace(
            "[authoring]",
            "[lint]\nids = [\"PROJ-[0-9a-f]{6}\", \"\\\\.logbook\"]\n\n[authoring]",
        );
    fs::write(fx.root.join("workflow.toml"), &workflow).unwrap();
    fs::write(fx.root.join("private.txt"), "secretword\n").unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let dest = fx.dest.to_str().unwrap();

    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "lint",
            dest,
            "--workflow",
            "workflow.toml",
            "--private",
            "private.txt",
            "--count",
        ],
    );
    assert!(!ok, "stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>() == ["reference", "4", "2"]),
        "{stdout}"
    );
    assert!(stderr.contains("lint: 4 finding(s)"), "{stderr}");

    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &[
            "lint",
            dest,
            "--workflow",
            "workflow.toml",
            "--private",
            "private.txt",
            "--keep-references",
        ],
    );
    assert!(ok, "stdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains("reference ids in text kept"), "{stdout}");

    fs::write(
        fx.root.join("workflow.toml"),
        workflow.replace("[lint]\n", "[lint]\nreferences = \"keep\"\n"),
    )
    .unwrap();
    let lint_count = [
        "lint",
        dest,
        "--workflow",
        "workflow.toml",
        "--private",
        "private.txt",
        "--count",
    ];
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &lint_count);
    assert!(ok, "stdout={stdout}\nstderr={stderr}");

    // A kept id in a path name still refuses.
    write(&fx.origin, "notes/PROJ-c0ffee.md", "x\n");
    git_in(&fx.home, &fx.origin, &["add", "notes/PROJ-c0ffee.md"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "a note"]);
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &lint_count);
    assert!(!ok, "stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>() == ["reference", "path", "1", "1"]),
        "{stdout}"
    );
}

// `strip_comments` removes source comments per language when a workflow lists it
// (off otherwise). It keeps shebangs, SPDX and licence headers, tool directives and doc comments, never touches strings,
// counts each file's removal in the receipt and leaves other languages alone.
#[test]
fn strip_comments_removes_source_comments_only_where_listed() {
    let fx = Fixture::new();
    write(
        &fx.origin,
        "src/lib.rs",
        "// SPDX-License-Identifier: MIT\n/// Docs.\npub fn f() -> &'static str {\n    // gone\n    \"// kept\" // gone\n}\n",
    );
    write(
        &fx.origin,
        "tools/run.py",
        "#!/usr/bin/env python3\n# gone\nprint('# kept')  # gone\n",
    );
    write(
        &fx.origin,
        "web/app.ts",
        "// @ts-expect-error kept\nconst u = 'http://x'; // gone\n",
    );
    write(
        &fx.origin,
        "notes.toml",
        "# a config comment stays\nx = 1\n",
    );
    git_in(&fx.home, &fx.origin, &["add", "."]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "sources"]);
    let workflow = fs::read_to_string(fx.root.join("workflow.toml")).unwrap();
    fs::write(
        fx.root.join("workflow.toml"),
        format!("{workflow}\n[[transformations]]\ntype = \"strip_comments\"\nexclude = [\"vendor/**\"]\n"),
    )
    .unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let show = |path: &str| git_out(&fx.home, &fx.dest, &["show", &format!("main:{path}")]);
    assert_eq!(
        show("src/lib.rs"),
        "// SPDX-License-Identifier: MIT\n/// Docs.\npub fn f() -> &'static str {\n    \"// kept\"\n}"
    );
    assert_eq!(
        show("tools/run.py"),
        "#!/usr/bin/env python3\nprint('# kept')"
    );
    assert_eq!(
        show("web/app.ts"),
        "// @ts-expect-error kept\nconst u = 'http://x';"
    );
    assert_eq!(show("notes.toml"), "# a config comment stays\nx = 1");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.root.join("fixture.receipt.json")).unwrap())
            .unwrap();
    let events = receipt["transform_events"].as_object().unwrap();
    let (label, count) = events
        .iter()
        .find(|(label, _)| label.ends_with("strip_comments"))
        .expect("a strip_comments count");
    assert_eq!(count.as_u64(), Some(3), "{label}: {events:?}");
}

// Any exported file a private pattern hits is stripped whole, counts only: `[lint] private_action = "strip"` judges the tree as it would be written (after every transformation and the private
// scrub) and drops each file whose name or text a private pattern hits, the lint's allowances notwithstanding (the gate
// allows no private term either). Events and stdout name pattern numbers and counts, never a term.
#[test]
fn lint_private_action_strip_drops_every_exported_file_a_private_pattern_hits() {
    let fx = Fixture::new();
    write(&fx.origin, "docs/a.md", "ran with Zorblax today\n");
    write(&fx.origin, "src/quuxhost7.rs", "pub fn clean() {}\n");
    write(&fx.origin, "docs/allowed/b.md", "Zorblax again\n");
    write(&fx.origin, "docs/clean.md", "nothing to see\n");
    write(&fx.origin, "docs/scrubbed.md", "built by scrubme-term\n");
    git_in(&fx.home, &fx.origin, &["add", "docs", "src"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "private hits"]);
    write(
        &fx.root,
        "private.txt",
        "# private\n(?i)zorblax\nquuxhost\\d\nscrubme-term\n",
    );
    write(&fx.root, "private-scrub.txt", "scrubme-term\tneutral\n");
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        format!(
            "{text}\n[lint]\nprivate = \"private.txt\"\nprivate_action = \"strip\"\n\n[[lint.allow]]\n\
             paths = [\"docs/allowed/**\"]\npatterns = [1]\n\n[scrub]\nprivate = \"private-scrub.txt\"\n"
        ),
    )
    .unwrap();

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "main"],
    );
    assert!(!listing.contains("docs/a.md"), "text hit\n{listing}");
    assert!(!listing.contains("quuxhost"), "name hit\n{listing}");
    assert!(
        !listing.contains("docs/allowed/b.md"),
        "allowance\n{listing}"
    );
    assert!(listing.contains("docs/clean.md"), "{listing}");
    assert!(
        listing.contains("docs/scrubbed.md"),
        "scrubbed first\n{listing}"
    );
    let scrubbed = git_out(&fx.home, &fx.dest, &["show", "main:docs/scrubbed.md"]);
    assert_eq!(scrubbed, "built by neutral");
    assert!(stdout.contains("private strip: 3 files"), "{stdout}");
    assert!(
        stdout.contains("path dropped (private pattern 1)")
            && stdout.contains("path dropped (private pattern 2)"),
        "{stdout}"
    );
    let lower = stdout.to_lowercase();
    assert!(
        !lower.contains("zorblax") && !lower.contains("quuxhost") && !lower.contains("scrubme"),
        "{stdout}"
    );
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert_eq!(receipt["transform_events"]["lint.private"], 3, "{receipt}");
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["lint", "dest"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");

    // Fail closed: stripping needs the private-patterns file, and the action is "refuse" or "strip".
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(&workflow, text.replace("private = \"private.txt\"\n", "")).unwrap();
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(!ok);
    assert!(stderr.contains("private_action"), "{stderr}");
    fs::write(
        &workflow,
        text.replace("private_action = \"strip\"", "private_action = \"scrub\""),
    )
    .unwrap();
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(!ok);
    assert!(stderr.contains("lint.private_action"), "{stderr}");
}

// The destination tree is written in a batch, not one `hash-object` and one `update-index`
// process per file (tens of thousands of processes for a large tree). A git shim on PATH counts the processes; the
// written tree keeps its bytes, modes and unusual names.
#[cfg(unix)]
#[test]
fn migrate_writes_the_destination_tree_in_a_batch() {
    let fx = Fixture::new();
    for n in 0..40 {
        write(
            &fx.origin,
            &format!("many/f{n:02}.txt"),
            &format!("file {n}\n"),
        );
    }
    write(&fx.origin, "many/with space.txt", "space\n");
    write(&fx.origin, "many/tab\tname.txt", "tab\n");
    write(&fx.origin, "many/run.sh", "#!/bin/sh\n");
    fs::set_permissions(
        fx.origin.join("many/run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("run.sh", fx.origin.join("many/link")).unwrap();
    git_in(&fx.home, &fx.origin, &["add", "many"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "many files"]);

    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH");
    let shim_dir = fx.root.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let log = fx.root.join("git-calls.log");
    fs::write(
        shim_dir.join("git"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real_git.display()
        ),
    )
    .unwrap();
    fs::set_permissions(shim_dir.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", shim_dir.display(), std::env::var("PATH").unwrap());
    let output = fx
        .cmd()
        .env("PATH", path)
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).unwrap();
    let per_file = calls
        .lines()
        .filter(|line| line.contains("hash-object") || line.contains("update-index"))
        .count();
    assert!(per_file <= 2, "{per_file} per-file git processes:\n{calls}");

    let tree = git_out(&fx.home, &fx.dest, &["ls-tree", "-r", "-z", "main", "many"]);
    assert!(tree.contains("100755 blob"), "{tree}");
    assert!(tree.contains("120000 blob"), "{tree}");
    assert!(tree.contains("\tmany/with space.txt\0"), "{tree:?}");
    assert!(tree.contains("\tmany/tab\tname.txt"), "{tree:?}");
    assert_eq!(tree.matches('\0').count(), 44, "{tree:?}");
    let body = git_out(&fx.home, &fx.dest, &["show", "main:many/f07.txt"]);
    assert_eq!(body, "file 7");
    let link = git_out(&fx.home, &fx.dest, &["cat-file", "-p", "main:many/link"]);
    assert_eq!(link, "run.sh");
}

// The gate reads a file that is not UTF-8 lossily, so a private term inside a binary (an image's text
// chunk, a font's name table) is a `private-term` finding. The strip judges such a file the same way, so a stripping
// export gates 0.
#[test]
fn lint_private_action_strip_judges_binary_files_as_the_gate_reads_them() {
    let fx = Fixture::new();
    let mut png = vec![0x89, b'P', b'N', b'G', 0xff, 0xfe, b'\n'];
    png.extend_from_slice(b"tEXtAuthor Zorblax\n");
    png.extend_from_slice(&[0x00, 0xc3, 0x28]);
    let path = fx.origin.join("assets/icon.png");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, &png).unwrap();
    fs::write(fx.origin.join("assets/clean.png"), [0x89, b'P', 0xff, 0x00]).unwrap();
    git_in(&fx.home, &fx.origin, &["add", "assets"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "binaries"]);
    write(&fx.root, "private.txt", "(?i)zorblax\n");
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        format!("{text}\n[lint]\nprivate = \"private.txt\"\nprivate_action = \"strip\"\n"),
    )
    .unwrap();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "main"],
    );
    assert!(!listing.contains("assets/icon.png"), "{listing}");
    assert!(listing.contains("assets/clean.png"), "{listing}");
    assert!(stdout.contains("private strip: 1 file"), "{stdout}");
}

// A whole-file strip leaves the export's Rust workspace unloadable (a member
// without its Cargo.toml, a `mod` without its file, an `include_str!` without its target). This fixture is a small
// cargo workspace where the private strip removes one module, one manifest and one included file.
fn cascade_fixture() -> Fixture {
    let fx = Fixture::new();
    write(
        &fx.origin,
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\n    \"crates/alpha\",\n    \"crates/beta\",\n    \"crates/gamma\",\n    \
         \"crates/delta\",\n    \"crates/eps\",\n]\n",
    );
    let manifest = |name: &str, extra: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}")
    };
    write(
        &fx.origin,
        "crates/alpha/Cargo.toml",
        &manifest("alpha", ""),
    );
    write(
        &fx.origin,
        "crates/alpha/src/lib.rs",
        "pub mod keep;\n\n/// The hidden half.\n#[cfg(test)]\nmod gone;\n",
    );
    write(&fx.origin, "crates/alpha/src/keep.rs", "pub fn keep() {}\n");
    write(
        &fx.origin,
        "crates/alpha/src/gone.rs",
        "// written for Zorblax\n",
    );
    write(
        &fx.origin,
        "crates/beta/Cargo.toml",
        &manifest(
            "beta",
            "\n[dependencies]\nalpha = { path = \"../alpha\" }\n",
        ),
    );
    write(
        &fx.origin,
        "crates/beta/src/lib.rs",
        "pub fn beta() { alpha::keep::keep() }\n",
    );
    write(
        &fx.origin,
        "crates/gamma/Cargo.toml",
        &manifest("gamma", "description = \"for Zorblax\"\n"),
    );
    write(&fx.origin, "crates/gamma/src/lib.rs", "pub fn gamma() {}\n");
    write(
        &fx.origin,
        "crates/delta/Cargo.toml",
        &manifest("delta", ""),
    );
    write(
        &fx.origin,
        "crates/delta/src/main.rs",
        "mod util;\n\nfn main() {\n    util::util();\n}\n",
    );
    write(&fx.origin, "crates/delta/src/util.rs", "pub fn util() {}\n");
    write(
        &fx.origin,
        "crates/delta/src/bin/tool.rs",
        "const DATA: &str = include_str!(\"../../data.txt\");\n\nfn main() {\n    print!(\"{DATA}\");\n}\n",
    );
    write(&fx.origin, "crates/delta/data.txt", "Zorblax\n");
    write(
        &fx.origin,
        "crates/eps/Cargo.toml",
        &manifest(
            "eps",
            "\n[dev-dependencies]\nalpha = { path = \"../alpha\" }\n",
        ),
    );
    write(&fx.origin, "crates/eps/src/lib.rs", "pub fn eps() {}\n");
    git_in(&fx.home, &fx.origin, &["add", "Cargo.toml", "crates"]);
    git_in(&fx.home, &fx.origin, &["commit", "-m", "a cargo workspace"]);
    write(&fx.root, "private.txt", "(?i)zorblax\n");
    fx
}

fn cascade_workflow(fx: &Fixture, extra: &str) {
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        format!("{text}\n[lint]\nprivate = \"private.txt\"\nprivate_action = \"strip\"\n{extra}"),
    )
    .unwrap();
}

fn verify_section(fx: &Fixture, on_fail: &str) -> String {
    format!(
        "\n[verify]\ncommands = [[\"cargo\", \"metadata\", \"--format-version\", \"1\", \"--no-deps\", \"--offline\"], \
         [\"cargo\", \"check\", \"--workspace\", \"--offline\", \"--quiet\"]]\ndir = \"verify-tree\"\non_fail = \"{on_fail}\"\n\
         env = {{ CARGO_TARGET_DIR = \"{}\" }}\n",
        fx.root.join("verify-target").display()
    )
}

// `[cascade] rust = "module"`: what a stripped file breaks is stripped too, up the module tree (a file declaring a
// missing module or including a missing file is stripped; a crate left without a library or any target is stripped
// whole, and so is a crate that depends on it), a member without its manifest leaves the workspace with its leftover
// files, and a dev-dependency on a stripped crate is unlinked. `[verify]` then runs `cargo metadata` and
// `cargo check --workspace` on the tree as it will be written, before anything is written.
#[test]
fn cascade_rust_module_strips_what_a_stripped_file_breaks_and_the_export_builds() {
    let fx = cascade_fixture();
    cascade_workflow(
        &fx,
        &format!(
            "\n[cascade]\nrust = \"module\"\n{}",
            verify_section(&fx, "refuse")
        ),
    );
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "main"],
    );
    for gone in [
        "crates/alpha/",
        "crates/beta/",
        "crates/gamma/",
        "crates/delta/src/bin/tool.rs",
    ] {
        assert!(
            !listing.contains(gone),
            "{gone} should be stripped\n{listing}"
        );
    }
    for kept in [
        "crates/delta/Cargo.toml",
        "crates/delta/src/main.rs",
        "crates/delta/src/util.rs",
        "crates/eps/Cargo.toml",
        "crates/eps/src/lib.rs",
    ] {
        assert!(listing.contains(kept), "{kept} should be kept\n{listing}");
    }
    let root = git_out(&fx.home, &fx.dest, &["show", "main:Cargo.toml"]);
    assert!(
        root.contains("\"crates/delta\"") && root.contains("\"crates/eps\""),
        "{root}"
    );
    assert!(
        !root.contains("alpha") && !root.contains("beta") && !root.contains("gamma"),
        "{root}"
    );
    let eps = git_out(&fx.home, &fx.dest, &["show", "main:crates/eps/Cargo.toml"]);
    assert!(!eps.contains("alpha"), "{eps}");
    assert!(stdout.contains("private strip: 3 files"), "{stdout}");
    assert!(stdout.contains("rust cascade: "), "{stdout}");
    assert!(stdout.contains("verify: 2 of 2 passed"), "{stdout}");
    assert!(!stdout.to_lowercase().contains("zorblax"), "{stdout}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert!(
        receipt["transform_events"]["cascade.rust"]
            .as_u64()
            .unwrap_or(0)
            >= 6,
        "{receipt}"
    );
    assert_eq!(receipt["verify"][1]["ok"], true, "{receipt}");
    assert!(fx.root.join("verify-tree/crates/eps/src/lib.rs").is_file());
}

// `[cascade] rust = "unlink"`: a stripped module takes its `mod` declaration (and the attributes and doc comment
// on it) with it, and the declaring file stays.
#[test]
fn cascade_rust_unlink_removes_the_declaration_of_a_stripped_module() {
    let fx = cascade_fixture();
    cascade_workflow(
        &fx,
        &format!(
            "\n[cascade]\nrust = \"unlink\"\n{}",
            verify_section(&fx, "refuse")
        ),
    );
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let lib = git_out(
        &fx.home,
        &fx.dest,
        &["show", "main:crates/alpha/src/lib.rs"],
    );
    assert_eq!(lib, "pub mod keep;", "{lib}");
    let listing = git_out(
        &fx.home,
        &fx.dest,
        &["ls-tree", "-r", "--name-only", "main"],
    );
    assert!(listing.contains("crates/beta/src/lib.rs"), "{listing}");
    assert!(!listing.contains("crates/gamma/"), "{listing}");
    assert!(stdout.contains("verify: 2 of 2 passed"), "{stdout}");
}

// `[verify]` refuses a tree its commands fail on, and nothing is written; `on_fail = "report"` writes it and records
// the failure in the receipt.
#[test]
fn verify_refuses_an_export_whose_workspace_does_not_load() {
    let fx = cascade_fixture();
    cascade_workflow(&fx, &verify_section(&fx, "refuse"));
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(!ok, "stdout={stdout}");
    assert!(
        stderr.contains("verify: command 1 of 2 (cargo metadata) failed"),
        "{stderr}"
    );
    assert!(
        git_head(&fx.home, &fx.dest).is_none(),
        "nothing should be written"
    );

    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap();
    fs::write(
        &workflow,
        text.replace("on_fail = \"refuse\"", "on_fail = \"report\""),
    )
    .unwrap();
    fs::remove_dir_all(fx.root.join("verify-tree")).ok();
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    assert!(stdout.contains("verify: 0 of 2 passed"), "{stdout}");
    let receipt = read_json(&fx.root.join("fixture.receipt.json"));
    assert_eq!(receipt["verify"][0]["ok"], false, "{receipt}");

    // Up to date: nothing to verify, and the run says so rather than counting 0 passes.
    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    assert!(
        stdout.contains("verify: skipped (no commit to verify)"),
        "{stdout}"
    );
}

// Every walked commit's header comes from one git stream, not one `git log -1` per commit.
#[test]
fn a_migrate_reads_every_commit_header_in_one_git_spawn() {
    let fx = Fixture::new();
    for n in 0..5 {
        commit_file(&fx, &format!("docs/{n}.md"), "x\n", &format!("commit {n}"));
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let real = path
        .split(':')
        .map(|dir| Path::new(dir).join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH");
    let shim = fx.root.join("shim");
    fs::create_dir_all(&shim).unwrap();
    let calls = fx.root.join("git-calls.log");
    let script = shim.join("git");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            calls.display(),
            real.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(bin())
        .env("HOME", &fx.home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("PATH", format!("{}:{path}", shim.display()))
        .current_dir(&fx.root)
        .args(["migrate", "workflow.toml"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&calls).unwrap();
    let per_commit = calls
        .lines()
        .filter(|line| line.contains(" log -1 --format=%an%n%ae%n%aI%n%B "))
        .count();
    assert_eq!(per_commit, 0, "no header is read one commit at a time");
    let streams = calls
        .lines()
        .filter(|line| line.contains(" log --stdin --no-walk=unsorted "))
        .count();
    assert_eq!(streams, 1, "{calls}");
    let message = git_out(&fx.home, &fx.dest, &["log", "-1", "--format=%B", "main"]);
    for n in 0..5 {
        assert!(message.contains(&format!("commit {n}")), "{message}");
    }
}

// A public export can start at a recent commit: `[origin] subjects = "clean"` reads
// the range's subjects before anything is walked, refuses a private term in one, never prints it, and names the
// commit to start after; `copybera subjects` reads the same and tries another start.
#[test]
fn origin_subjects_clean_refuses_a_private_subject_and_names_the_start_after_it() {
    let fx = Fixture::new();
    write(&fx.root, "private.txt", "hostname-zebra\\d\n");
    let workflow = fx.root.join("workflow.toml");
    let text = fs::read_to_string(&workflow).unwrap().replacen(
        "ref = \"main\"\n",
        "ref = \"main\"\nsubjects = \"clean\"\n",
        1,
    );
    fs::write(
        &workflow,
        format!("{text}\n[lint]\nprivate = \"private.txt\"\n"),
    )
    .unwrap();
    commit_file(
        &fx,
        "docs/a.md",
        "a\n",
        "moved the build to hostname-zebra7",
    );
    let dirty = git_out(&fx.home, &fx.origin, &["rev-parse", "HEAD"]);
    commit_file(&fx, "docs/b.md", "b\n", "a clean subject");

    let (ok, stdout, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(!ok, "{stdout}");
    assert!(
        stderr.contains("origin.subjects: 1 of 5 subjects in the root.."),
        "{stderr}"
    );
    assert!(stderr.contains("private pattern 1: 1 subject"), "{stderr}");
    assert!(
        stderr.contains(&format!("[origin] since = \"{dirty}\"")),
        "{stderr}"
    );
    assert!(!stderr.contains("zebra") && !stdout.contains("zebra"));
    assert!(
        !stderr.contains("loading origin commit"),
        "refused before the walk"
    );
    assert_eq!(
        git_out(&fx.home, &fx.dest, &["for-each-ref", "--count=1"]),
        "",
        "nothing written"
    );

    let (ok, stdout, _) = run_in(&fx, &fx.root, &["subjects", "workflow.toml"]);
    assert!(!ok);
    assert!(
        stdout.contains("subjects: 5 commits in the root.."),
        "{stdout}"
    );
    assert!(stdout.contains("private pattern 1: 1 subject"), "{stdout}");
    assert!(!stdout.contains("zebra"));
    let (ok, stdout, stderr) = run_in(
        &fx,
        &fx.root,
        &["subjects", "workflow.toml", "--since", &dirty],
    );
    assert!(ok, "{stderr}");
    assert!(stdout.contains("subjects: 1 commit in "), "{stdout}");
    assert!(stdout.contains("ok: subjects clean"), "{stdout}");

    let text = fs::read_to_string(&workflow).unwrap().replacen(
        "subjects = \"clean\"\n",
        &format!("subjects = \"clean\"\nsince = \"{dirty}\"\n"),
        1,
    );
    fs::write(&workflow, text).unwrap();
    let (ok, _, stderr) = run_in(&fx, &fx.root, &["migrate", "workflow.toml"]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("origin.subjects: 1 subject in "),
        "{stderr}"
    );
}
