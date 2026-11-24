//! The workflows under `examples/` parse, and each does what its comments say: the examples are part of the test
//! suite, so an example that stops parsing fails the build.

use std::collections::BTreeMap;
use std::path::PathBuf;

use copybera_rules::gate::GateConfig;
use copybera_rules::{apply, parse_workflow, Blob, Commit, Mode, StubRewriter, Workflow};

fn example(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn run(wf: &Workflow, message: &str, files: &[(&str, &str)]) -> (String, BTreeMap<String, String>) {
    let commit = Commit {
        id: "a".repeat(40),
        message: message.to_string(),
        author_name: "Ada Lovelace".to_string(),
        author_email: "ada@corp.example".to_string(),
        author_date: None,
        files: files
            .iter()
            .map(|(p, b)| (p.to_string(), Blob::from(b.as_bytes())))
            .collect(),
        ..Default::default()
    };
    let result = apply(wf, vec![commit], BTreeMap::new(), &StubRewriter);
    let out = &result.commits[0];
    (
        out.message.clone(),
        out.files
            .iter()
            .map(|(p, b)| (p.clone(), String::from_utf8(b.to_vec()).unwrap()))
            .collect(),
    )
}

#[test]
fn every_example_workflow_parses() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut parsed = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if path.extension().is_some_and(|ext| ext == "toml") && name != "gate.toml" {
            let text = std::fs::read_to_string(&path).unwrap();
            parse_workflow(&text).unwrap_or_else(|err| panic!("{name}: {err}"));
            parsed.push(name);
        }
    }
    parsed.sort();
    assert_eq!(parsed, ["mirror-iterative.toml", "publish-subtree.toml"]);
}

#[test]
fn publish_subtree_squashes_one_folder_and_strips_tickets_and_notes() {
    let wf = parse_workflow(&example("publish-subtree.toml")).unwrap();
    assert_eq!(wf.name, "publish-subtree");
    assert_eq!(wf.mode, Mode::Squash);
    assert_eq!(wf.destination.push, "main");
    // The lint refuses what the strip removes; no [verify] runs unless the user enables it.
    assert_eq!(wf.lint_references().0, ["TICKET-[0-9]+"]);
    assert!(wf.verify.is_none());
    assert!(wf.origin_paths.allows("libs/parser-internal/src/lib.rs"));
    assert!(!wf.origin_paths.allows("apps/web/src/main.ts"));
    assert!(!wf.origin_paths.allows("libs/parser-internal/keys.secret"));

    let lib = "// see TICKET-99 for the plan\npub fn parse() {}\n";
    let (message, files) = run(
        &wf,
        "parser: faster tokens (TICKET-1234)",
        &[
            ("libs/parser-internal/src/lib.rs", lib),
            ("libs/parser-internal/NOTES.md", "internal plan\n"),
            ("README.md", "Talk to internal.corp.example.\n"),
        ],
    );
    assert_eq!(message, "parser: faster tokens");
    assert!(
        !files.contains_key("libs/parser-internal/NOTES.md"),
        "{files:?}"
    );
    assert!(!files.contains_key("libs/parser/NOTES.md"), "{files:?}");
    let lib = &files["libs/parser/src/lib.rs"];
    assert!(!lib.contains("TICKET-"), "{lib}");
    assert!(lib.contains("pub fn parse() {}"), "{lib}");
    assert_eq!(files["README.md"], "Talk to example.com.\n");
}

#[test]
fn mirror_iterative_renames_the_docs_folder() {
    let wf = parse_workflow(&example("mirror-iterative.toml")).unwrap();
    assert_eq!(wf.mode, Mode::Iterative);
    assert!(wf.origin_paths.allows("docs/guide/start.md"));
    assert!(!wf.origin_paths.allows("docs/drafts/next.md"));
    let (_, files) = run(
        &wf,
        "docs: start page",
        &[("docs/guide/start.md", "# Start\n")],
    );
    assert!(files.contains_key("site/guide/start.md"), "{files:?}");
}

#[test]
fn the_example_gate_parses_with_its_allowance() {
    let root: toml::Table = example("gate.toml").parse().unwrap();
    let gate = match root.get("gate") {
        Some(toml::Value::Table(table)) => table,
        _ => panic!("examples/gate.toml holds no [gate] table"),
    };
    let config = GateConfig::parse(gate).unwrap();
    assert_eq!(config.allow.len(), 1);
    assert_eq!(config.allow[0].class, "email");
}
