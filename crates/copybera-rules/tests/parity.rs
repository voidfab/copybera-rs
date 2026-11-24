//! The features a project rename export needs: a scoped replace_text, ordered literal pairs, path moves, a strip by
//! content, file modes and the gate's false positives. One section per feature.

use std::collections::BTreeMap;

use copybera_rules::{apply, parse_workflow, Blob, Commit, StubRewriter, Workflow};

fn workflow(transformations: &str) -> Workflow {
    parse_workflow(&format!(
        r#"
[workflow]
name = "parity"
mode = "squash"
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
default = {{ name = "Authors", email = "a@example.invalid" }}
{transformations}
"#
    ))
    .unwrap()
}

fn commit(message: &str, files: &[(&str, &str)]) -> Commit {
    Commit {
        id: "a".repeat(40),
        message: message.to_string(),
        author_name: "Ada".to_string(),
        author_email: "ada@example.invalid".to_string(),
        author_date: None,
        files: files
            .iter()
            .map(|(p, b)| (p.to_string(), Blob::from(b.as_bytes())))
            .collect(),
        ..Default::default()
    }
}

fn run(wf: &Workflow, message: &str, files: &[(&str, &str)]) -> (String, BTreeMap<String, String>) {
    let result = apply(
        wf,
        vec![commit(message, files)],
        BTreeMap::new(),
        &StubRewriter,
    );
    let out = &result.commits[0];
    (
        out.message.clone(),
        out.files
            .iter()
            .map(|(p, b)| (p.clone(), String::from_utf8(b.to_vec()).unwrap()))
            .collect(),
    )
}

// ---- 1. A scoped replace_text: `paths` / `exclude` choose the files, `ignore_lines` keeps a line verbatim. An export
// keeps the attribution set's bytes (licence, notices, changelog, decision records) and skips a line that already
// reads the new name (a one-release alias line). ----

#[test]
fn replace_text_paths_and_exclude_choose_the_files() {
    let wf = workflow(
        r#"
[[transformations]]
type = "replace_text"
pattern = 'oldname'
replacement = "rebrand"
on = "both"
paths = ["docs/**", "README.md"]
exclude = ["docs/adr/**"]
"#,
    );
    let (message, files) = run(
        &wf,
        "oldname subject",
        &[
            ("README.md", "oldname readme\n"),
            ("docs/guide.md", "oldname guide\n"),
            ("docs/adr/0042.md", "oldname adr\n"),
            ("src/lib.rs", "// oldname code\n"),
        ],
    );
    assert_eq!(files["README.md"], "rebrand readme\n");
    assert_eq!(files["docs/guide.md"], "rebrand guide\n");
    assert_eq!(files["docs/adr/0042.md"], "oldname adr\n", "excluded");
    assert_eq!(files["src/lib.rs"], "// oldname code\n", "outside paths");
    // The scope is a set of files: a message is not a path, `on` alone decides it.
    assert_eq!(message, "rebrand subject");
}

#[test]
fn replace_text_ignore_lines_keeps_a_matching_line_verbatim() {
    let wf = workflow(
        r#"
[[transformations]]
type = "replace_text"
pattern = 'oldname'
replacement = "rebrand"
on = "files"
ignore_lines = ['(?i)rebrand']
"#,
    );
    let (_, files) = run(
        &wf,
        "m",
        &[(
            "a.md",
            "oldname one\nalias: oldname -> rebrand\r\noldname three",
        )],
    );
    assert_eq!(
        files["a.md"],
        "rebrand one\nalias: oldname -> rebrand\r\nrebrand three"
    );
}

#[test]
fn replace_text_scope_globs_and_line_regexes_are_validated() {
    let base = r#"
[workflow]
name = "parity"
mode = "squash"
[origin]
path = "o"
ref = "main"
[destination]
path = "d"
fetch = "main"
push = "main"
[paths.origin]
include = ["**"]
[paths.destination]
include = ["**"]
[authoring]
default = { name = "A", email = "a@example.invalid" }
[[transformations]]
type = "replace_text"
pattern = 'x'
replacement = "y"
on = "files"
"#;
    let err = parse_workflow(&format!("{base}paths = [\"a/[\"]\n"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("transformations[0].paths") && err.contains("invalid glob"),
        "{err}"
    );
    let err = parse_workflow(&format!("{base}ignore_lines = [\"(\"]\n"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("transformations[0].ignore_lines") && err.contains("invalid regex"),
        "{err}"
    );
}

// ---- 2. replace_pairs: ordered literal pairs with copybara's sentinel semantics in one transformation. A pair never
// matches text an earlier pair wrote, and the ignore_lines guard is judged on the line as it came in, so a line the
// first pair rewrote to the new name is not skipped by the next pair's alias guard. ----

#[test]
fn replace_pairs_never_rematch_what_an_earlier_pair_wrote() {
    let wf = workflow(
        r#"
[[transformations]]
type = "replace_pairs"
on = "files"
pairs = [["oldname", "rebrand"], ["rebrand", "WRONG"], ["com.oldname.", "com.example.rebrand."]]
"#,
    );
    let (_, files) = run(&wf, "m", &[("a.txt", "oldname rebrand com.oldname.app\n")]);
    // `com.oldname.` comes after `oldname` in the list, so `oldname` took it first: the order is the author's.
    assert_eq!(files["a.txt"], "rebrand WRONG com.rebrand.app\n");
}

#[test]
fn replace_pairs_judge_the_alias_guard_on_the_incoming_line() {
    let wf = workflow(
        r#"
[[transformations]]
type = "replace_pairs"
on = "both"
ignore_lines = ['(?i)rebrand']
paths = ["**/*.md"]
pairs = [["acme-admin", "rebrand-admin"], ["oldname", "rebrand"], ["Oldname", "Rebrand"]]
"#,
    );
    let (message, files) = run(
        &wf,
        "Oldname: acme-admin and oldname",
        &[
            (
                "docs/a.md",
                "acme-admin and oldname\nalias oldname = rebrand\nOldname\n",
            ),
            ("src/a.rs", "oldname\n"),
        ],
    );
    assert_eq!(
        files["docs/a.md"],
        "rebrand-admin and rebrand\nalias oldname = rebrand\nRebrand\n"
    );
    assert_eq!(files["src/a.rs"], "oldname\n", "outside paths");
    assert_eq!(message, "Rebrand: rebrand-admin and rebrand");
}

#[test]
fn replace_pairs_are_validated() {
    for (pairs, want) in [
        ("[[\"a\"]]", "transformations[0].pairs[0]"),
        ("[[\"\", \"b\"]]", "transformations[0].pairs[0]"),
        ("[]", "transformations[0].pairs"),
    ] {
        let err = parse_workflow(&format!(
            r#"
[workflow]
name = "p"
mode = "squash"
[origin]
path = "o"
ref = "main"
[destination]
path = "d"
fetch = "main"
push = "main"
[paths.origin]
include = ["**"]
[paths.destination]
include = ["**"]
[authoring]
default = {{ name = "A", email = "a@example.invalid" }}
[[transformations]]
type = "replace_pairs"
on = "files"
pairs = {pairs}
"#
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(want) && !err.contains("unknown type"),
            "{pairs}: {err}"
        );
    }
}

// ---- 3. move_paths: a path whose segments carry a pair moves to its new name, unless
// the new name is taken by another path or by a second move; replace_pairs `follow_moves` makes the
// text references follow, through the same sentinel pass as the family pairs. ----

#[test]
fn move_paths_renames_segments_and_leaves_a_taken_name_in_place() {
    let wf = workflow(
        r#"
[[transformations]]
type = "move_paths"
pairs = [["oldname", "rebrand"], ["acme", "rebrand"], ["Acme", "Rebrand"]]
exclude = ["benchmarks/results/**"]
"#,
    );
    let result = apply(
        &wf,
        vec![commit(
            "m",
            &[
                ("apps/acme-admin/main.rs", "a"),
                ("tools/AcmeProbe.swift", "b"),
                ("benchmarks/results/acme-run.md", "c"),
                ("fixtures/oldname.json", "d"),
                ("fixtures/rebrand.json", "e"),
                ("pair/acme.txt", "f"),
                ("pair/oldname.txt", "g"),
            ],
        )],
        BTreeMap::new(),
        &StubRewriter,
    );
    let paths: Vec<&str> = result.commits[0].files.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        [
            "apps/rebrand-admin/main.rs",
            "benchmarks/results/acme-run.md",
            "fixtures/oldname.json",
            "fixtures/rebrand.json",
            "pair/acme.txt",
            "pair/oldname.txt",
            "tools/RebrandProbe.swift",
        ]
    );
    let stays = result
        .stripped
        .iter()
        .filter(|e| e.detail.starts_with("stays: target taken"))
        .count();
    assert_eq!(stays, 3, "{:?}", result.stripped);
}

#[test]
fn replace_pairs_follow_moves_rewrites_references_to_a_moved_name() {
    let wf = workflow(
        r#"
[[transformations]]
type = "move_paths"
pairs = [["oldname", "rebrand"], ["acme", "rebrand"], ["Acme", "Rebrand"]]

[[transformations]]
type = "replace_pairs"
on = "files"
follow_moves = true
ignore_lines = ['(?i)rebrand']
pairs = [["oldname", "rebrand"]]
"#,
    );
    let (_, files) = run(
        &wf,
        "m",
        &[
            ("tools/AcmeProbe.swift", "struct AcmeProbe {}\n"),
            ("apps/acme-admin/main.rs", "fn main() {}\n"),
            ("docs/acme/index.md", "x\n"),
            (
                "README.md",
                "see AcmeProbe and acme-admin, oldname\nacme stays a word\n",
            ),
        ],
    );
    assert_eq!(
        files["tools/RebrandProbe.swift"],
        "struct RebrandProbe {}\n"
    );
    // the bare family name (`docs/acme/`) is a folder move, not a text pair: `acme` in prose is left to the pairs
    assert_eq!(
        files["README.md"],
        "see RebrandProbe and rebrand-admin, rebrand\nacme stays a word\n"
    );
    assert!(files.contains_key("docs/rebrand/index.md"));
}

// ---- 4. strip_matching: a covered file whose text matches a pattern is stripped, not scrubbed (a record that must
// keep its wording is not published when it is flagged). The event names the pattern's number, never
// the matched text. ----

#[test]
fn strip_matching_strips_a_flagged_file_inside_its_paths_only() {
    let wf = workflow(
        r#"
[[transformations]]
type = "strip_matching"
paths = ["benchmarks/results/**"]
exclude = ["benchmarks/results/README.md"]
patterns = ['/Users/[a-z]+/', '\b10\.1\.0\.[0-9]+\b']

[[transformations]]
type = "replace_text"
pattern = '/Users/[a-z]+/'
replacement = "/Users/you/"
on = "files"
"#,
    );
    let result = apply(
        &wf,
        vec![commit(
            "m",
            &[
                ("benchmarks/results/run-a.md", "ran on /Users/alice/x\n"),
                (
                    "benchmarks/results/run-b.json",
                    "{\"host\": \"10.1.0.7\"}\n",
                ),
                ("benchmarks/results/run-c.md", "clean\n"),
                ("benchmarks/results/README.md", "/Users/alice/ example\n"),
                ("docs/guide.md", "cd /Users/alice/src\n"),
            ],
        )],
        BTreeMap::new(),
        &StubRewriter,
    );
    let files = &result.commits[0].files;
    let paths: Vec<&str> = files.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        [
            "benchmarks/results/README.md",
            "benchmarks/results/run-c.md",
            "docs/guide.md"
        ]
    );
    // outside the strip's paths the later scrub still runs
    assert_eq!(&files["docs/guide.md"][..], b"cd /Users/you/src\n");
    assert!(result
        .files_stripped
        .contains("benchmarks/results/run-a.md"));
    let events: Vec<&str> = result
        .stripped
        .iter()
        .filter(|e| e.transform.ends_with("strip_matching"))
        .map(|e| e.detail.as_str())
        .collect();
    assert_eq!(
        events,
        ["path dropped (pattern 1)", "path dropped (pattern 2)"]
    );
}

#[test]
fn strip_matching_needs_paths_and_patterns() {
    let base = r#"
[workflow]
name = "p"
mode = "squash"
[origin]
path = "o"
ref = "main"
[destination]
path = "d"
fetch = "main"
push = "main"
[paths.origin]
include = ["**"]
[paths.destination]
include = ["**"]
[authoring]
default = { name = "A", email = "a@example.invalid" }
[[transformations]]
type = "strip_matching"
"#;
    for (extra, want) in [
        ("patterns = ['x']\n", "transformations[0].paths"),
        ("paths = ['**']\n", "transformations[0].patterns"),
        ("paths = ['**']\npatterns = ['(']\n", "invalid regex"),
    ] {
        let err = parse_workflow(&format!("{base}{extra}"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(want) && !err.contains("unknown type"),
            "{extra}: {err}"
        );
    }
}

// ---- 6. Modes: a moved path takes its mode along, a stripped path drops it. ----

#[test]
fn a_moved_path_keeps_its_mode_and_a_stripped_one_drops_it() {
    let wf = workflow(
        r#"
[[transformations]]
type = "strip_paths"
globs = ["gone/**"]

[[transformations]]
type = "move_paths"
pairs = [["acme", "rebrand"]]
"#,
    );
    let mut input = commit(
        "m",
        &[("bin/acme-run", "x"), ("gone/tool", "y"), ("link", "bin")],
    );
    for (path, mode) in [
        ("bin/acme-run", "100755"),
        ("gone/tool", "100755"),
        ("link", "120000"),
    ] {
        input.modes.insert(path.to_string(), mode.to_string());
    }
    let result = apply(&wf, vec![input], BTreeMap::new(), &StubRewriter);
    let modes: Vec<(&str, &str)> = result.commits[0]
        .modes
        .iter()
        .map(|(p, m)| (p.as_str(), m.as_str()))
        .collect();
    assert_eq!(modes, [("bin/rebrand-run", "100755"), ("link", "120000")]);
}

// ---- 8. Gate false positives: a source folder named `home` or `Users` is not a home folder, and a leftover name on a one-release alias line (it also reads the new
// name) is not a finding when `leftover_ignore_lines` says so. ----

#[test]
fn the_gate_skips_folders_named_home_and_alias_lines() {
    use copybera_rules::gate::{scan, GateConfig};
    let table: toml::Table = toml::from_str(
        r#"
placeholders = ["you"]
leftover = ['(?i)oldname']
leftover_ignore_lines = ['(?i)rebrand']
"#,
    )
    .unwrap();
    let config = GateConfig::parse(&table).unwrap();
    let files: BTreeMap<String, Blob> = [(
        "a.md",
        "fixtures/home/Library/x and src/Users/Shared/y\n\
             a real one: /Users/alice/src and \"/home/bob/.config\"\n\
             alias: oldname -> rebrand\nleft: oldname\n",
    )]
    .into_iter()
    .map(|(p, b)| (p.to_string(), Blob::from(b.as_bytes())))
    .collect();
    let report = scan(&files, &config, &[]);
    assert_eq!(report.counts["home-path"].hits, 2, "{report:?}");
    assert_eq!(report.counts["leftover"].hits, 1, "{report:?}");
}
