//! `copybera closure`: print, or check a workflow against, the source closure of named packages at an
//! origin revision (`copybera_rules::closure`).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Result};
use copybera_rules::closure::{closure, source_filter, Closure, ClosureRequest};

use crate::git::Git;

pub struct ClosureArgs<'a> {
    pub origin: &'a Path,
    pub rev: &'a str,
    pub request: ClosureRequest,
    pub check: Option<&'a Path>,
    pub why: bool,
}

pub fn run(args: ClosureArgs<'_>, git: &Git, out: &mut impl Write) -> Result<()> {
    if args.request.cargo.is_empty() && args.request.node.is_empty() {
        return Err(anyhow!(
            "closure: name at least one --cargo or --node package folder"
        ));
    }
    if !git.is_repo(args.origin) {
        return Err(anyhow!(
            "closure: {} is not a git repository",
            args.origin.display()
        ));
    }
    let sha = git.rev_parse(args.origin, &format!("{}^{{commit}}", args.rev))?;
    let paths: BTreeSet<String> = git.list_paths(args.origin, &sha)?.into_iter().collect();
    let loaded =
        git.load_trees_filtered_once(args.origin, std::slice::from_ref(&sha), &source_filter())?;
    let files = loaded.trees.into_iter().next().unwrap_or_default();
    let found = closure(&paths, &files, &args.request).map_err(|e| anyhow!("closure: {e}"))?;
    let include = found.include();
    let short = &sha[..sha.len().min(12)];
    match args.check {
        None => {
            if args.why {
                writeln!(out, "include = [")?;
                for entry in &include {
                    let why = found.entries.get(entry).map(String::as_str).unwrap_or("");
                    writeln!(out, "  {}, # {why}", toml_quote(entry))?;
                }
                writeln!(out, "]")?;
            } else {
                write!(out, "{}", found.include_toml())?;
            }
            eprintln!(
                "closure: {} in {}, {}; {} at {short}",
                plural(found.crates.len(), "crate"),
                plural(found.workspaces.len(), "workspace"),
                plural(found.node_packages.len(), "JavaScript package"),
                plural(include.len(), "include entry"),
            );
            Ok(())
        }
        Some(workflow) => check(workflow, &found, &include, &paths, short, out),
    }
}

fn check(
    workflow: &Path,
    found: &Closure,
    include: &[String],
    paths: &BTreeSet<String>,
    short: &str,
    out: &mut impl Write,
) -> Result<()> {
    let wf = crate::run::validate(workflow)?;
    let filter = &wf.origin_paths;
    let (mut covered, mut left_out) = (0usize, 0usize);
    let mut missing: Vec<(String, usize, usize)> = Vec::new();
    for entry in include {
        let named = Closure::paths_of(entry, paths);
        let mut miss = 0usize;
        for path in &named {
            if filter.exclude_set.is_match(path.as_str()) {
                left_out += 1;
            } else if filter.include_set.is_match(path.as_str()) {
                covered += 1;
            } else {
                miss += 1;
            }
        }
        if miss > 0 {
            missing.push((entry.clone(), miss, named.len()));
        }
    }
    if missing.is_empty() {
        writeln!(
            out,
            "closure check: covered ({}, {}, {} left out by the workflow's exclude) at {short}",
            plural(include.len(), "entry"),
            plural(covered, "path"),
            left_out
        )?;
        return Ok(());
    }
    for (entry, miss, total) in &missing {
        let why = found.entries.get(entry).map(String::as_str).unwrap_or("");
        writeln!(out, "missing {entry} ({miss} of {total} paths; {why})")?;
    }
    Err(anyhow!(
        "closure check: {} missing from {}'s include at {short}",
        plural(missing.len(), "entry"),
        workflow.display()
    ))
}

fn toml_quote(s: &str) -> String {
    format!("{:?}", s)
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else if let Some(stem) = noun.strip_suffix('y') {
        format!("{count} {stem}ies")
    } else {
        format!("{count} {noun}s")
    }
}
