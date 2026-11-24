//! `[cascade] rust`: a whole-file strip leaves a Rust tree that cargo cannot load or compile (a workspace
//! member without its `Cargo.toml`, a `mod` whose file is gone, an `include_str!` whose target is gone, a crate whose
//! path dependency is gone). This pass walks the tree as it will be written and repairs or strips what is broken,
//! until nothing more changes:
//!
//! - a workspace member (literal) without a manifest leaves `members` / `default-members`, and its leftover files are
//!   stripped; a glob member that matches a folder without a manifest goes to `exclude`;
//! - a `[workspace.dependencies]` path entry whose crate is gone is removed;
//! - a crate whose normal or build path dependency is gone (or has no library left), whose `[lib]` or `build` file is
//!   gone, or that has no library and no binary left, is stripped whole (never the root package);
//! - a dev-dependency on a gone crate, and a `[[bin]]` / `[[test]]` / `[[example]]` / `[[bench]]` whose file is gone,
//!   are removed from the manifest;
//! - a file that `include_str!` / `include_bytes!` / `include!`s a missing file is stripped;
//! - a `mod name;` whose file is gone: in `"module"` mode the declaring file is stripped (and so on up the module
//!   tree); in `"unlink"` mode the declaration (with its attributes and doc comment) and the single-line `use name::`
//!   items of that file are removed; in `"auto"` mode it is unlinked when nothing else in the crate names the
//!   module, and the declaring file is stripped otherwise;
//! - a crate whose library was in the tree before the strip (`before`) and is gone now is stripped whole: its
//!   binaries, tests and examples use it.
//!
//! The pass is pure: it reads and edits the in-memory tree. Its events name the path acted on and the cause; a missing
//! target is never named (its name may be the reason it was stripped).

use std::collections::{BTreeMap, BTreeSet};

use globset::GlobBuilder;
use regex::Regex;

use crate::transform::Blob;

/// What a `mod name;` whose file is gone does to the declaring file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustCascade {
    /// The declaring file is stripped, and the cascade goes on up the module tree.
    Module,
    /// The declaration is removed and the declaring file stays.
    Unlink,
    /// The declaration is removed when nothing else in the crate names the module (no `name::` path and no
    /// `::name`); otherwise the declaring file is stripped, as in `Module`.
    Auto,
}

impl RustCascade {
    pub fn as_str(self) -> &'static str {
        match self {
            RustCascade::Module => "module",
            RustCascade::Unlink => "unlink",
            RustCascade::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeEvent {
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CascadeSummary {
    pub files_stripped: usize,
    pub crates_stripped: usize,
    pub members_pruned: usize,
    pub declarations_unlinked: usize,
    pub manifest_edits: usize,
}

/// Runs the cascade over one tree. `files` and `modes` are a commit's tree; every stripped path is also returned in
/// the events (one per file).
pub fn cascade_rust(
    files: &mut BTreeMap<String, Blob>,
    modes: &mut BTreeMap<String, String>,
    before: &BTreeSet<String>,
    mode: RustCascade,
    events: &mut Vec<CascadeEvent>,
) -> CascadeSummary {
    cascade_rust_in(files, modes, before, None, mode, events)
}

/// `cascade_rust` that also knows every path of the origin commit (`origin`): an include of a file the origin never
/// had is left alone.
pub fn cascade_rust_in(
    files: &mut BTreeMap<String, Blob>,
    modes: &mut BTreeMap<String, String>,
    before: &BTreeSet<String>,
    origin: Option<&BTreeSet<String>>,
    mode: RustCascade,
    events: &mut Vec<CascadeEvent>,
) -> CascadeSummary {
    let mut summary = CascadeSummary::default();
    let mut state = Tree {
        files,
        modes,
        before,
        origin,
    };
    // Each round strips or edits at least one thing or stops; the tree is finite, so this ends.
    loop {
        let mut round = Round::default();
        state.workspaces(&mut round);
        if round.apply(&mut state, events, &mut summary) {
            continue;
        }
        state.packages(mode, &mut round);
        if !round.apply(&mut state, events, &mut summary) {
            break;
        }
    }
    summary
}

struct Tree<'a> {
    files: &'a mut BTreeMap<String, Blob>,
    modes: &'a mut BTreeMap<String, String>,
    /// The paths before the strip that started the cascade.
    before: &'a BTreeSet<String>,
    /// Every path of the origin commit, path filter or not, when the caller has it.
    origin: Option<&'a BTreeSet<String>>,
}

#[derive(Default)]
struct Round {
    /// path -> cause
    strip: BTreeMap<String, String>,
    crates: BTreeMap<String, String>,
    /// path -> new bytes, cause
    edits: BTreeMap<String, (Vec<u8>, Vec<String>)>,
    members_pruned: usize,
    declarations_unlinked: usize,
}

impl Round {
    fn edit(&mut self, path: &str, bytes: Vec<u8>, cause: String) {
        let entry = self
            .edits
            .entry(path.to_string())
            .or_insert_with(|| (Vec::new(), Vec::new()));
        entry.0 = bytes;
        entry.1.push(cause);
    }

    fn apply(
        &mut self,
        tree: &mut Tree<'_>,
        events: &mut Vec<CascadeEvent>,
        summary: &mut CascadeSummary,
    ) -> bool {
        let mut changed = false;
        for (dir, cause) in std::mem::take(&mut self.crates) {
            let owned = strippable_files(tree.files, &dir);
            if owned.is_empty() {
                continue;
            }
            summary.crates_stripped += 1;
            for path in owned {
                self.strip
                    .entry(path)
                    .or_insert_with(|| format!("crate stripped: {cause}"));
            }
        }
        for (path, (bytes, causes)) in std::mem::take(&mut self.edits) {
            if self.strip.contains_key(&path) || !tree.files.contains_key(&path) {
                continue;
            }
            if tree.files.get(&path).map(|held| &held[..]) != Some(&bytes[..]) {
                tree.files.insert(path.clone(), Blob::from(bytes));
                summary.manifest_edits += 1;
                changed = true;
                for cause in causes {
                    events.push(CascadeEvent {
                        path: path.clone(),
                        detail: cause,
                    });
                }
            }
        }
        for (path, cause) in std::mem::take(&mut self.strip) {
            if tree.files.remove(&path).is_some() {
                tree.modes.remove(&path);
                summary.files_stripped += 1;
                changed = true;
                events.push(CascadeEvent {
                    path,
                    detail: cause,
                });
            }
        }
        summary.members_pruned += std::mem::take(&mut self.members_pruned);
        summary.declarations_unlinked += std::mem::take(&mut self.declarations_unlinked);
        changed
    }
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path)
}

fn in_dir(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// `dir` joined with a relative path, `.` and `..` resolved; `None` when it leaves the tree or is absolute.
fn join(dir: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn manifest_of(dir: &str) -> String {
    in_dir(dir, "Cargo.toml")
}

/// The files under `dir` whose nearest package folder is `dir` (a nested package keeps its own files).
fn owned_files(files: &BTreeMap<String, Blob>, dir: &str) -> Vec<String> {
    let nested: Vec<String> = files
        .keys()
        .filter(|p| file_name(p) == "Cargo.toml" && under(p, dir) && dir_of(p) != dir)
        .map(|p| dir_of(p).to_string())
        .collect();
    files
        .keys()
        .filter(|p| under(p, dir) && !nested.iter().any(|n| under(p, n)))
        .cloned()
        .collect()
}

/// The files a crate strip (or a leftover strip) takes from `dir`: its own files, but not a licence or notice file
/// whose folder still holds a nested package (vendored or patched crates keep the licence beside them).
fn strippable_files(files: &BTreeMap<String, Blob>, dir: &str) -> Vec<String> {
    let notice = Regex::new(r"(?i)^(NOTICE|LICEN[CS]E|COPYING|ATTRIBUTION)([-_.].*)?$")
        .expect("the notice-file pattern compiles");
    let nested: Vec<&str> = files
        .keys()
        .filter(|p| file_name(p) == "Cargo.toml" && under(p, dir) && dir_of(p) != dir)
        .map(|p| dir_of(p))
        .collect();
    owned_files(files, dir)
        .into_iter()
        .filter(|p| !(notice.is_match(file_name(p)) && nested.iter().any(|n| under(n, dir_of(p)))))
        .collect()
}

fn parse_toml(bytes: &[u8]) -> Option<toml::Table> {
    std::str::from_utf8(bytes).ok()?.parse::<toml::Table>().ok()
}

fn edit_toml(bytes: &[u8], f: impl FnOnce(&mut toml_edit::DocumentMut) -> bool) -> Option<Vec<u8>> {
    let mut doc = std::str::from_utf8(bytes)
        .ok()?
        .parse::<toml_edit::DocumentMut>()
        .ok()?;
    f(&mut doc).then(|| doc.to_string().into_bytes())
}

const DEP_TABLES: [&str; 3] = ["dependencies", "build-dependencies", "dev-dependencies"];

struct Package {
    dir: String,
    name: String,
    table: toml::Table,
    lib: Option<String>,
    lib_declared_missing: bool,
    /// The library was in the tree before the strip and is gone now.
    lib_lost: bool,
}

impl Tree<'_> {
    fn has(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn dirs(&self) -> BTreeSet<String> {
        let mut dirs = BTreeSet::new();
        for path in self.files.keys() {
            let mut dir = dir_of(path);
            while !dir.is_empty() && dirs.insert(dir.to_string()) {
                dir = dir_of(dir);
            }
        }
        dirs
    }

    fn manifests(&self) -> Vec<(String, toml::Table)> {
        self.files
            .iter()
            .filter(|(p, _)| file_name(p) == "Cargo.toml")
            .filter_map(|(p, bytes)| Some((dir_of(p).to_string(), parse_toml(bytes)?)))
            .collect()
    }

    /// Workspace members, default members, excludes and `[workspace.dependencies]`.
    fn workspaces(&self, round: &mut Round) {
        let dirs = self.dirs();
        for (ws_dir, table) in self.manifests() {
            let Some(ws) = table.get("workspace").and_then(|v| v.as_table()) else {
                continue;
            };
            let manifest = manifest_of(&ws_dir);
            let members = str_array(ws.get("members"));
            let excluded: BTreeSet<String> = str_array(ws.get("exclude"))
                .into_iter()
                .filter_map(|e| join(&ws_dir, &e))
                .collect();
            let mut drop_members = Vec::new();
            let mut add_excludes = Vec::new();
            for member in &members {
                if member.contains(['*', '?', '[']) {
                    let Ok(glob) = GlobBuilder::new(member).literal_separator(true).build() else {
                        continue;
                    };
                    let matcher = glob.compile_matcher();
                    for dir in &dirs {
                        let Some(rel) = rel_to(dir, &ws_dir) else {
                            continue;
                        };
                        if matcher.is_match(rel)
                            && !self.has(&manifest_of(dir))
                            && !excluded.contains(dir)
                        {
                            add_excludes.push(rel.to_string());
                        }
                    }
                } else if let Some(dir) = join(&ws_dir, member) {
                    if !self.has(&manifest_of(&dir)) && !excluded.contains(&dir) {
                        drop_members.push(member.clone());
                        for path in strippable_files(self.files, &dir) {
                            round.strip.insert(
                                path,
                                "leftover of a workspace member without its Cargo.toml".to_string(),
                            );
                        }
                    }
                }
            }
            let defaults = str_array(ws.get("default-members"));
            let drop_defaults: Vec<String> = defaults
                .iter()
                .filter(|d| {
                    join(&ws_dir, d).is_none_or(|dir| !self.has(&manifest_of(&dir)))
                        && !d.contains(['*', '?', '['])
                })
                .cloned()
                .collect();
            let drop_deps: Vec<String> = ws
                .get("dependencies")
                .and_then(|v| v.as_table())
                .map(|deps| {
                    deps.iter()
                        .filter(|(_, spec)| {
                            path_of(spec)
                                .and_then(|p| join(&ws_dir, &p))
                                .is_some_and(|dir| !self.has(&manifest_of(&dir)))
                        })
                        .map(|(name, _)| name.clone())
                        .collect()
                })
                .unwrap_or_default();
            if drop_members.is_empty()
                && add_excludes.is_empty()
                && drop_defaults.is_empty()
                && drop_deps.is_empty()
            {
                continue;
            }
            let bytes = &self.files[&manifest];
            let edited = edit_toml(bytes, |doc| {
                let Some(ws) = doc.get_mut("workspace").and_then(|w| w.as_table_like_mut()) else {
                    return false;
                };
                for (key, gone) in [
                    ("members", &drop_members),
                    ("default-members", &drop_defaults),
                ] {
                    if let Some(array) = ws.get_mut(key).and_then(|v| v.as_array_mut()) {
                        array.retain(|v| v.as_str().is_none_or(|s| !gone.iter().any(|g| g == s)));
                    }
                }
                if !add_excludes.is_empty() {
                    if ws.get("exclude").is_none() {
                        ws.insert("exclude", toml_edit::value(toml_edit::Array::new()));
                    }
                    if let Some(array) = ws.get_mut("exclude").and_then(|v| v.as_array_mut()) {
                        for e in &add_excludes {
                            if !array.iter().any(|v| v.as_str() == Some(e)) {
                                array.push(e.as_str());
                            }
                        }
                    }
                }
                if let Some(deps) = ws
                    .get_mut("dependencies")
                    .and_then(|v| v.as_table_like_mut())
                {
                    for name in &drop_deps {
                        deps.remove(name);
                    }
                }
                true
            });
            if let Some(edited) = edited {
                round.members_pruned += drop_members.len() + add_excludes.len();
                for m in &drop_members {
                    round.edit(
                        &manifest,
                        edited.clone(),
                        format!("workspace member {m} pruned (no Cargo.toml)"),
                    );
                }
                for e in &add_excludes {
                    round.edit(
                        &manifest,
                        edited.clone(),
                        format!("folder {e} excluded from a glob member (no Cargo.toml)"),
                    );
                }
                for d in &drop_defaults {
                    round.edit(
                        &manifest,
                        edited.clone(),
                        format!("default member {d} pruned (no Cargo.toml)"),
                    );
                }
                for name in &drop_deps {
                    round.edit(
                        &manifest,
                        edited.clone(),
                        format!("workspace dependency {name} removed (its crate is gone)"),
                    );
                }
            }
        }
    }

    fn packages(&self, mode: RustCascade, round: &mut Round) {
        let manifests = self.manifests();
        // Workspace dependency names with a path, by workspace folder.
        let mut ws_path_deps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        // Every `[workspace.dependencies]` name, by workspace folder: an inherited dependency whose entry is
        // gone (removed with its crate) is gone too.
        let mut ws_dep_names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (dir, table) in &manifests {
            if table.contains_key("workspace") {
                ws_dep_names.entry(dir.clone()).or_default();
            }
        }
        for (dir, table) in &manifests {
            if let Some(deps) = table
                .get("workspace")
                .and_then(|w| w.get("dependencies"))
                .and_then(|d| d.as_table())
            {
                let entry = ws_path_deps.entry(dir.clone()).or_default();
                ws_dep_names
                    .entry(dir.clone())
                    .or_default()
                    .extend(deps.keys().cloned());
                for (name, spec) in deps {
                    if let Some(p) = path_of(spec).and_then(|p| join(dir, &p)) {
                        entry.insert(name.clone(), p);
                    }
                }
            }
        }
        let ws_dirs: Vec<String> = manifests
            .iter()
            .filter(|(_, t)| t.contains_key("workspace"))
            .map(|(d, _)| d.clone())
            .collect();
        let mut packages = Vec::new();
        for (dir, table) in &manifests {
            let Some(pkg) = table.get("package").and_then(|p| p.as_table()) else {
                continue;
            };
            let name = pkg
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let declared = table
                .get("lib")
                .and_then(|l| l.get("path"))
                .and_then(|p| p.as_str())
                .map(|p| join(dir, p));
            let lib_path = match &declared {
                Some(path) => path.clone(),
                None => Some(in_dir(dir, "src/lib.rs")),
            };
            let lib_lost = lib_path
                .as_ref()
                .is_some_and(|p| self.before.contains(p) && !self.has(p));
            let (lib, lib_declared_missing) = match declared {
                Some(Some(path)) if self.has(&path) => (Some(path), false),
                Some(_) => (None, true),
                None => {
                    let path = in_dir(dir, "src/lib.rs");
                    (self.has(&path).then_some(path), false)
                }
            };
            packages.push(Package {
                dir: dir.clone(),
                name,
                table: table.clone(),
                lib,
                lib_declared_missing,
                lib_lost,
            });
        }
        let with_lib: BTreeSet<&str> = packages
            .iter()
            .filter(|p| p.lib.is_some())
            .map(|p| p.dir.as_str())
            .collect();
        let package_dirs: BTreeSet<&str> = packages.iter().map(|p| p.dir.as_str()).collect();

        for package in &packages {
            let dir = package.dir.as_str();
            let manifest = manifest_of(dir);
            let mut broken: Option<String> = None;
            if package.lib_lost {
                broken = Some("its library was stripped".to_string());
            } else if package.lib_declared_missing {
                broken = Some("its [lib] file is gone".to_string());
            }
            if let Some(build) = package
                .table
                .get("package")
                .and_then(|p| p.get("build"))
                .and_then(|b| b.as_str())
            {
                if join(dir, build).is_none_or(|p| !self.has(&p)) {
                    broken.get_or_insert_with(|| "its build script is gone".to_string());
                }
            }
            // Path dependencies, plain and through the workspace.
            let ws = ws_dirs
                .iter()
                .filter(|w| under(&manifest, w))
                .max_by_key(|w| w.len());
            let mut dev_unlink: Vec<(Vec<String>, String)> = Vec::new();
            for (tables, kind, spec_name, spec) in dependency_specs(&package.table) {
                let inherited = spec.get("workspace").and_then(|w| w.as_bool()) == Some(true);
                let inherited_gone = inherited
                    && ws
                        .and_then(|w| ws_dep_names.get(w))
                        .is_some_and(|names| !names.contains(&spec_name));
                let target = if inherited {
                    ws.and_then(|w| ws_path_deps.get(w))
                        .and_then(|deps| deps.get(&spec_name).cloned())
                } else {
                    path_of(&spec).and_then(|p| join(dir, &p))
                };
                let (gone, no_lib) = if inherited_gone {
                    (true, false)
                } else {
                    let Some(target) = target else {
                        continue;
                    };
                    let gone = !package_dirs.contains(target.as_str());
                    (gone, !gone && !with_lib.contains(target.as_str()))
                };
                if !(gone || no_lib) {
                    continue;
                }
                if kind == "dev-dependencies" {
                    dev_unlink.push((tables, spec_name));
                } else if broken.is_none() {
                    broken = Some(if gone {
                        format!("its dependency {spec_name} is gone")
                    } else {
                        format!("its dependency {spec_name} has no library left")
                    });
                }
            }
            if let Some(cause) = broken {
                if dir.is_empty() {
                    continue; // never strip the root package
                }
                round.crates.insert(dir.to_string(), cause);
                continue;
            }
            // Targets.
            let owned = owned_files(self.files, dir);
            let owned_set: BTreeSet<&str> = owned.iter().map(String::as_str).collect();
            let mut roots: BTreeSet<String> = BTreeSet::new();
            let mut bins = 0usize;
            if let Some(lib) = &package.lib {
                roots.insert(lib.clone());
            }
            for path in &owned {
                let Some(rel) = rel_to(path, dir) else {
                    continue;
                };
                let auto_bin = rel == "src/main.rs" || is_target_file(rel, "src/bin");
                if auto_bin {
                    bins += 1;
                }
                if auto_bin
                    || rel == "build.rs"
                    || is_target_file(rel, "tests")
                    || is_target_file(rel, "examples")
                    || is_target_file(rel, "benches")
                {
                    roots.insert(path.clone());
                }
            }
            let mut drop_targets: Vec<(String, usize, String)> = Vec::new();
            for kind in ["bin", "test", "example", "bench"] {
                let Some(targets) = package.table.get(kind).and_then(|t| t.as_array()) else {
                    continue;
                };
                for (index, target) in targets.iter().enumerate() {
                    let declared = target.get("path").and_then(|p| p.as_str());
                    let name = target.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let candidates: Vec<String> = match declared {
                        Some(p) => join(dir, p).into_iter().collect(),
                        None => default_target_paths(kind, name, &package.name)
                            .into_iter()
                            .map(|rel| in_dir(dir, &rel))
                            .collect(),
                    };
                    match candidates.iter().find(|c| self.has(c)) {
                        Some(found) => {
                            roots.insert(found.clone());
                            if kind == "bin" {
                                bins += 1;
                            }
                        }
                        None => drop_targets.push((kind.to_string(), index, name.to_string())),
                    }
                }
            }
            if package.lib.is_none() && bins == 0 && !dir.is_empty() {
                round
                    .crates
                    .insert(dir.to_string(), "no library or binary left".to_string());
                continue;
            }
            if !drop_targets.is_empty() || !dev_unlink.is_empty() {
                let edited = edit_toml(&self.files[&manifest], |doc| {
                    for (kind, index, _) in drop_targets.iter().rev() {
                        if let Some(array) =
                            doc.get_mut(kind).and_then(|v| v.as_array_of_tables_mut())
                        {
                            array.remove(*index);
                        }
                    }
                    for kind in ["bin", "test", "example", "bench"] {
                        if doc
                            .get(kind)
                            .and_then(|v| v.as_array_of_tables())
                            .is_some_and(|a| a.is_empty())
                        {
                            doc.remove(kind);
                        }
                    }
                    for (tables, name) in &dev_unlink {
                        let mut item: Option<&mut toml_edit::Item> = None;
                        for (i, key) in tables.iter().enumerate() {
                            item = if i == 0 {
                                doc.get_mut(key)
                            } else {
                                item.and_then(|it| it.get_mut(key))
                            };
                        }
                        if let Some(t) = item.and_then(|it| it.as_table_like_mut()) {
                            t.remove(name);
                        }
                    }
                    true
                });
                if let Some(edited) = edited {
                    for (kind, _, name) in &drop_targets {
                        round.edit(
                            &manifest,
                            edited.clone(),
                            format!("[[{kind}]] {name} removed (its file is gone)"),
                        );
                    }
                    for (_, name) in &dev_unlink {
                        round.edit(
                            &manifest,
                            edited.clone(),
                            format!("dev-dependency {name} unlinked (its crate is gone)"),
                        );
                    }
                }
                continue; // read the edited manifest next round
            }
            // Sources.
            let mut refs: Option<Vec<(String, String, usize)>> = None;
            for path in owned.iter().filter(|p| p.ends_with(".rs")) {
                if under(path, &in_dir(dir, "target")) {
                    continue;
                }
                let Ok(text) = std::str::from_utf8(&self.files[path]) else {
                    continue;
                };
                let scan = scan_rust(text);
                let file_dir = dir_of(path);
                let is_root = roots.contains(path) || file_name(path) == "mod.rs";
                let base = if is_root {
                    file_dir.to_string()
                } else {
                    in_dir(file_dir, file_name(path).trim_end_matches(".rs"))
                };
                // An include whose target the origin never had either (code behind a feature, say) is not the
                // export's doing and is left to the build.
                if scan.includes.iter().any(|inc| {
                    join(file_dir, inc).is_some_and(|p| {
                        !self.has(&p)
                            && !self.dir_exists(&p)
                            && self.origin.is_none_or(|origin| {
                                origin.contains(&p) || {
                                    let prefix = format!("{p}/");
                                    origin
                                        .range(prefix.clone()..)
                                        .next()
                                        .is_some_and(|q| q.starts_with(&prefix))
                                }
                            })
                    })
                }) {
                    round
                        .strip
                        .insert(path.clone(), "includes a file that is gone".to_string());
                    continue;
                }
                let mut orphans = Vec::new();
                for decl in &scan.mods {
                    let candidates: Vec<String> = match (&decl.path_attr, decl.inline.is_empty()) {
                        (Some(p), true) => join(file_dir, p).into_iter().collect(),
                        (Some(_), false) => continue, // a #[path] inside an inline module: not judged
                        (None, _) => {
                            let mut b = base.clone();
                            for seg in &decl.inline {
                                b = in_dir(&b, seg);
                            }
                            vec![
                                in_dir(&b, &format!("{}.rs", decl.name)),
                                in_dir(&b, &format!("{}/mod.rs", decl.name)),
                            ]
                        }
                    };
                    let present = candidates.iter().any(|c| self.has(c));
                    // A module whose file the origin never had either (behind a feature, say) is not the export's
                    // doing.
                    let origin_had = self
                        .origin
                        .is_none_or(|origin| candidates.iter().any(|c| origin.contains(c)));
                    if !present && origin_had && owned_set.contains(path.as_str()) {
                        orphans.push(decl);
                    }
                }
                if orphans.is_empty() {
                    continue;
                }
                match mode {
                    RustCascade::Module => {
                        round.strip.insert(
                            path.clone(),
                            "declares a module whose file is gone".to_string(),
                        );
                    }
                    RustCascade::Auto => {
                        let refs = refs.get_or_insert_with(|| {
                            owned
                                .iter()
                                .filter(|p| p.ends_with(".rs"))
                                .filter_map(|p| {
                                    let text = std::str::from_utf8(&self.files[p]).ok()?;
                                    Some(
                                        path_idents(text)
                                            .into_iter()
                                            .map(|(ident, line)| (p.clone(), ident, line))
                                            .collect::<Vec<_>>(),
                                    )
                                })
                                .flatten()
                                .collect()
                        });
                        // A `use` of the module counts as naming it: the items it imports (or re-exports) are used.
                        // A `#[cfg(test)]` module never does: only tests name it, and the build has none.
                        let named = unlink_lines(text, &orphans).is_none()
                            || refs.iter().any(|(p, ident, line)| {
                                orphans.iter().filter(|d| !d.cfg_test).any(|d| {
                                    &d.name == ident
                                        && !(p == path
                                            && (d.first_line..=d.last_line).contains(line))
                                })
                            });
                        if named {
                            round.strip.insert(
                                path.clone(),
                                "declares a module whose file is gone, and the crate still names it"
                                    .to_string(),
                            );
                        } else if let Some(next) = unlink(text, &orphans) {
                            round.declarations_unlinked += orphans.len();
                            round.edit(
                                path,
                                next.into_bytes(),
                                format!(
                                    "{} declaration(s) of a module whose file is gone unlinked",
                                    orphans.len()
                                ),
                            );
                        }
                    }
                    RustCascade::Unlink => match unlink(text, &orphans) {
                        Some(next) => {
                            round.declarations_unlinked += orphans.len();
                            round.edit(
                                path,
                                next.into_bytes(),
                                format!(
                                    "{} declaration(s) of a module whose file is gone unlinked",
                                    orphans.len()
                                ),
                            );
                        }
                        None => {
                            round.strip.insert(
                                path.clone(),
                                "declares a module whose file is gone (not unlinkable)".to_string(),
                            );
                        }
                    },
                }
            }
        }
    }

    fn dir_exists(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        self.files
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(p, _)| p.starts_with(&prefix))
    }
}

fn rel_to<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        Some(path)
    } else {
        path.strip_prefix(dir)?.strip_prefix('/')
    }
}

/// `src/bin/x.rs`, `src/bin/x/main.rs` (and the same under `tests`, `examples`, `benches`).
fn is_target_file(rel: &str, folder: &str) -> bool {
    let Some(rest) = rel.strip_prefix(folder).and_then(|r| r.strip_prefix('/')) else {
        return false;
    };
    match rest.split_once('/') {
        None => rest.ends_with(".rs"),
        Some((_, tail)) => tail == "main.rs",
    }
}

fn default_target_paths(kind: &str, name: &str, package: &str) -> Vec<String> {
    let folder = match kind {
        "bin" => "src/bin",
        "test" => "tests",
        "example" => "examples",
        _ => "benches",
    };
    let mut paths = vec![
        format!("{folder}/{name}.rs"),
        format!("{folder}/{name}/main.rs"),
    ];
    if kind == "bin" && name == package {
        paths.insert(0, "src/main.rs".to_string());
    }
    paths
}

fn str_array(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn path_of(spec: &toml::Value) -> Option<String> {
    spec.get("path")
        .and_then(|p| p.as_str())
        .map(str::to_string)
}

/// Every dependency entry: (table keys from the document root, table kind, name, spec).
fn dependency_specs(table: &toml::Table) -> Vec<(Vec<String>, &'static str, String, toml::Value)> {
    let mut out = Vec::new();
    let mut walk = |prefix: Vec<String>, t: &toml::Table| {
        for kind in DEP_TABLES {
            if let Some(deps) = t.get(kind).and_then(|d| d.as_table()) {
                for (name, spec) in deps {
                    let mut keys = prefix.clone();
                    keys.push(kind.to_string());
                    out.push((keys, kind, name.clone(), spec.clone()));
                }
            }
        }
    };
    walk(Vec::new(), table);
    if let Some(targets) = table.get("target").and_then(|t| t.as_table()) {
        for (cfg, t) in targets {
            if let Some(t) = t.as_table() {
                walk(vec!["target".to_string(), cfg.clone()], t);
            }
        }
    }
    out
}

// ---- a small Rust lexer: enough to find `mod name;`, inline `mod name { .. }`, `#[path = ".."]` and include macros
// outside comments and string literals.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Punct(char),
    Str(String),
}

fn lex(text: &str) -> Vec<(Tok, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 0usize;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let mut depth = 0;
            while i < n {
                if chars[i] == '/' && i + 1 < n && chars[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if c == '"' || ((c == 'b' || c == 'c') && i + 1 < n && chars[i + 1] == '"') {
            let start_line = line;
            if c != '"' {
                i += 1;
            }
            i += 1;
            let mut s = String::new();
            while i < n && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < n {
                    s.push(chars[i]);
                    s.push(chars[i + 1]);
                    if chars[i + 1] == '\n' {
                        line += 1;
                    }
                    i += 2;
                    continue;
                }
                if chars[i] == '\n' {
                    line += 1;
                }
                s.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push((Tok::Str(s), start_line));
        } else if (c == 'r' || ((c == 'b' || c == 'c') && i + 1 < n && chars[i + 1] == 'r')) && {
            let j = if c == 'r' { i + 1 } else { i + 2 };
            let mut k = j;
            while k < n && chars[k] == '#' {
                k += 1;
            }
            k < n && chars[k] == '"' && (k > j || chars[j] == '"')
        } {
            let start_line = line;
            let mut j = if c == 'r' { i + 1 } else { i + 2 };
            let mut hashes = 0;
            while chars[j] == '#' {
                hashes += 1;
                j += 1;
            }
            j += 1; // the quote
            let mut s = String::new();
            loop {
                if j >= n {
                    break;
                }
                if chars[j] == '"' && (0..hashes).all(|h| j + 1 + h < n && chars[j + 1 + h] == '#')
                {
                    j += 1 + hashes;
                    break;
                }
                if chars[j] == '\n' {
                    line += 1;
                }
                s.push(chars[j]);
                j += 1;
            }
            i = j;
            out.push((Tok::Str(s), start_line));
        } else if c == '\'' {
            // A char literal ('x', '\n', '\u{..}') or a lifetime ('a).
            if i + 2 < n && chars[i + 1] == '\\' {
                let mut j = i + 2;
                while j < n && chars[j] != '\'' && chars[j] != '\n' {
                    j += 1;
                }
                i = j + 1;
            } else if i + 2 < n && chars[i + 2] == '\'' {
                i += 3;
            } else {
                i += 1;
                while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
            }
        } else if c.is_alphabetic() || c == '_' {
            let mut s = String::new();
            if c == 'r'
                && i + 2 < n
                && chars[i + 1] == '#'
                && (chars[i + 2].is_alphabetic() || chars[i + 2] == '_')
            {
                i += 2;
            }
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                s.push(chars[i]);
                i += 1;
            }
            out.push((Tok::Ident(s), line));
        } else if c.is_ascii_digit() {
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                if chars[i] == '.' && !(i + 1 < n && chars[i + 1].is_ascii_digit()) {
                    break;
                }
                i += 1;
            }
        } else {
            out.push((Tok::Punct(c), line));
            i += 1;
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModDecl {
    name: String,
    /// Inline modules around the declaration, outermost first.
    inline: Vec<String>,
    path_attr: Option<String>,
    /// Under `#[cfg(test)]`, its own or an enclosing item's: never part of a non-test build.
    cfg_test: bool,
    /// First line of the item (its attributes and visibility) and the line of its `;`, 0-based.
    first_line: usize,
    last_line: usize,
}

#[derive(Debug, Default)]
struct Scan {
    mods: Vec<ModDecl>,
    /// Include targets outside `#[cfg(test)]` code (a test-only include never breaks the build).
    includes: Vec<String>,
}

/// An attribute's tokens (from `#` to its `]`) say `cfg(test)`, alone or inside `all(..)` / `any(..)`, and not under `not(..)`.
fn is_cfg_test(attr: &[(Tok, usize)]) -> bool {
    let idents: Vec<&str> = attr
        .iter()
        .filter_map(|(t, _)| match t {
            Tok::Ident(id) => Some(id.as_str()),
            _ => None,
        })
        .collect();
    idents.first() == Some(&"cfg") && idents.contains(&"test") && !idents.contains(&"not")
}

fn scan_rust(text: &str) -> Scan {
    let toks = lex(text);
    let mut scan = Scan::default();
    let mut depth = 0usize;
    let mut inline: Vec<(String, usize)> = Vec::new();
    let mut item_start: Option<usize> = None;
    let mut path_attr: Option<String> = None;
    // An outer `#[cfg(test)]` waiting for its item, the depths of the test blocks we are inside, and an
    // inner `#![cfg(test)]` (the whole file).
    let mut cfg_test_pending = false;
    let mut test_depths: Vec<usize> = Vec::new();
    let mut file_test = false;
    let mut i = 0;
    while i < toks.len() {
        let (tok, line) = &toks[i];
        if item_start.is_none() {
            item_start = Some(*line);
        }
        match tok {
            Tok::Punct('{') => {
                if std::mem::take(&mut cfg_test_pending) {
                    test_depths.push(depth);
                }
                depth += 1;
                item_start = None;
                path_attr = None;
            }
            Tok::Punct('}') => {
                depth = depth.saturating_sub(1);
                if inline.last().is_some_and(|(_, d)| *d == depth) {
                    inline.pop();
                }
                if test_depths.last() == Some(&depth) {
                    test_depths.pop();
                }
                item_start = None;
                path_attr = None;
            }
            Tok::Punct(';') => {
                item_start = None;
                path_attr = None;
                cfg_test_pending = false;
            }
            Tok::Punct('#') => {
                // #[path = "..."]
                if let (
                    Some((Tok::Punct('['), _)),
                    Some((Tok::Ident(id), _)),
                    Some((Tok::Punct('='), _)),
                    Some((Tok::Str(s), _)),
                    Some((Tok::Punct(']'), _)),
                ) = (
                    toks.get(i + 1),
                    toks.get(i + 2),
                    toks.get(i + 3),
                    toks.get(i + 4),
                    toks.get(i + 5),
                ) {
                    if id == "path" {
                        path_attr = Some(s.clone());
                        i += 6;
                        continue;
                    }
                }
                // Skip an attribute's brackets so `{`/`;` inside it are not structure.
                let mut j = i + 1;
                if matches!(toks.get(j), Some((Tok::Punct('!'), _))) {
                    j += 1;
                }
                if matches!(toks.get(j), Some((Tok::Punct('['), _))) {
                    let mut level = 0;
                    while j < toks.len() {
                        match toks[j].0 {
                            Tok::Punct('[') => level += 1,
                            Tok::Punct(']') => {
                                level -= 1;
                                if level == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    let inner = matches!(toks.get(i + 1), Some((Tok::Punct('!'), _)));
                    if is_cfg_test(&toks[i..=j.min(toks.len() - 1)]) {
                        if inner {
                            file_test = true;
                        } else {
                            cfg_test_pending = true;
                        }
                    }
                    if inner {
                        item_start = None;
                    }
                    i = j + 1;
                    continue;
                }
            }
            Tok::Ident(id) if id == "mod" => {
                if let Some((Tok::Ident(name), _)) = toks.get(i + 1) {
                    match toks.get(i + 2) {
                        Some((Tok::Punct(';'), end)) => {
                            scan.mods.push(ModDecl {
                                name: name.clone(),
                                inline: inline.iter().map(|(n, _)| n.clone()).collect(),
                                path_attr: path_attr.take(),
                                cfg_test: std::mem::take(&mut cfg_test_pending)
                                    || file_test
                                    || !test_depths.is_empty(),
                                first_line: item_start.unwrap_or(*line),
                                last_line: *end,
                            });
                            item_start = None;
                            i += 3;
                            continue;
                        }
                        Some((Tok::Punct('{'), _)) => {
                            if std::mem::take(&mut cfg_test_pending) {
                                test_depths.push(depth);
                            }
                            inline.push((name.clone(), depth));
                            depth += 1;
                            item_start = None;
                            path_attr = None;
                            i += 3;
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            Tok::Ident(id) if id == "include_str" || id == "include_bytes" || id == "include" => {
                if let (
                    Some((Tok::Punct('!'), _)),
                    Some((Tok::Punct('('), _)),
                    Some((Tok::Str(s), _)),
                    Some((Tok::Punct(')'), _)),
                ) = (
                    toks.get(i + 1),
                    toks.get(i + 2),
                    toks.get(i + 3),
                    toks.get(i + 4),
                ) {
                    if !s.contains('\\') && !s.contains('{') && !file_test && test_depths.is_empty()
                    {
                        scan.includes.push(s.clone());
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    scan
}

/// Removes each orphan declaration (its lines, with the doc comment above them) and the single-line `use` items
/// that name it. `None` when a declaration shares a line with other code.
fn unlink(text: &str, orphans: &[&ModDecl]) -> Option<String> {
    let drop = unlink_lines(text, orphans)?;
    Some(
        text.split_inclusive('\n')
            .zip(drop)
            .filter(|(_, d)| !d)
            .map(|(l, _)| l)
            .collect(),
    )
}

/// Identifiers next to a `::` (`name::x`, `crate::name`) or inside a `use` item (`use crate::{name, ..};`), with
/// their 0-based line: the names a path can use.
fn path_idents(text: &str) -> Vec<(String, usize)> {
    let toks = lex(text);
    let colons = |i: usize| {
        matches!(toks.get(i), Some((Tok::Punct(':'), _)))
            && matches!(toks.get(i + 1), Some((Tok::Punct(':'), _)))
    };
    let mut out = Vec::new();
    let mut in_use = false;
    for (i, (tok, line)) in toks.iter().enumerate() {
        match tok {
            Tok::Ident(id) if id == "use" => in_use = true,
            Tok::Punct(';') => in_use = false,
            Tok::Ident(id) => {
                if in_use || colons(i + 1) || (i >= 2 && colons(i - 2)) {
                    out.push((id.clone(), *line));
                }
            }
            _ => {}
        }
    }
    out
}

/// The lines `unlink` removes: each declaration's lines with the doc comment above them, and the single-line
/// `use name::..` items of the file. `None` when a declaration shares a line with other code.
fn unlink_lines(text: &str, orphans: &[&ModDecl]) -> Option<Vec<bool>> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut drop = vec![false; lines.len()];
    let mut names = Vec::new();
    for decl in orphans {
        let first = lines.get(decl.first_line)?;
        let last = lines.get(decl.last_line)?;
        let first_ok = first.trim_start().starts_with(['#', 'p', 'm']);
        let after = last
            .split_once(';')
            .map(|(_, rest)| rest.trim())
            .unwrap_or("x");
        if !first_ok || !(after.is_empty() || after.starts_with("//")) {
            return None;
        }
        for d in drop
            .iter_mut()
            .take(decl.last_line + 1)
            .skip(decl.first_line)
        {
            *d = true;
        }
        let mut above = decl.first_line;
        while above > 0 && lines[above - 1].trim_start().starts_with("///") {
            above -= 1;
            drop[above] = true;
        }
        if decl.inline.is_empty() {
            names.push(decl.name.clone());
        }
    }
    if !names.is_empty() {
        let alternatives = names
            .iter()
            .map(|n| regex::escape(n))
            .collect::<Vec<_>>()
            .join("|");
        let rx = Regex::new(&format!(
            r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?use\s+(?:self::)?(?:{alternatives})(?:::|\s*;|\s+as\s)[^;]*;\s*(?://.*)?$"
        ))
        .ok()?;
        for (index, line) in lines.iter().enumerate() {
            if rx.is_match(line.trim_end_matches(['\n', '\r'])) {
                drop[index] = true;
            }
        }
    }
    Some(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(entries: &[(&str, &str)]) -> BTreeMap<String, Blob> {
        entries
            .iter()
            .map(|(p, b)| (p.to_string(), Blob::from(b.as_bytes())))
            .collect()
    }

    fn run(
        files: &mut BTreeMap<String, Blob>,
        mode: RustCascade,
    ) -> (CascadeSummary, Vec<CascadeEvent>) {
        let mut modes = BTreeMap::new();
        let mut events = Vec::new();
        let before: BTreeSet<String> = files.keys().cloned().collect();
        let summary = cascade_rust(files, &mut modes, &before, mode, &mut events);
        (summary, events)
    }

    #[test]
    fn the_lexer_skips_comments_strings_and_lifetimes() {
        let scan = scan_rust(
            "// mod a;\n/* mod b; /* nested */ mod c; */\nconst S: &str = \"mod d;\";\nconst R: &str = r#\"mod e;\"#;\n\
             fn f<'a>(x: &'a str) -> char { 'x' }\nmod real;\n#[path = \"other.rs\"]\npub(crate) mod p;\n\
             mod outer {\n    mod inner;\n}\nmod after;\nstatic D: &[u8] = include_bytes!(\"../data.bin\");\n",
        );
        let names: Vec<(&str, Vec<String>, Option<String>)> = scan
            .mods
            .iter()
            .map(|m| (m.name.as_str(), m.inline.clone(), m.path_attr.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("real", vec![], None),
                ("p", vec![], Some("other.rs".to_string())),
                ("inner", vec!["outer".to_string()], None),
                ("after", vec![], None),
            ]
        );
        assert_eq!(scan.includes, vec!["../data.bin".to_string()]);
        assert_eq!(scan.mods[1].first_line, 6);
        assert_eq!(scan.mods[1].last_line, 7);
    }

    #[test]
    fn an_inline_module_and_a_path_attribute_resolve_like_rustc() {
        let mut files = tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n"),
            (
                "a/Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
            ),
            (
                "a/src/lib.rs",
                "mod outer {\n    mod inner;\n}\n#[path = \"elsewhere/x.rs\"]\nmod x;\nmod y;\n",
            ),
            ("a/src/outer/inner.rs", ""),
            ("a/src/elsewhere/x.rs", ""),
            ("a/src/y/mod.rs", "mod z;\n"),
            ("a/src/y/z.rs", ""),
        ]);
        let before = files.clone();
        let (summary, events) = run(&mut files, RustCascade::Module);
        assert_eq!(summary, CascadeSummary::default(), "{events:?}");
        assert_eq!(files, before);
    }

    #[test]
    fn module_mode_cascades_up_to_the_crate_and_its_dependents() {
        let mut files = tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\nlow = { path = \"crates/low\" }\n"),
            ("crates/low/Cargo.toml", "[package]\nname = \"low\"\nversion = \"0.1.0\"\n"),
            ("crates/low/src/lib.rs", "mod a;\n"),
            ("crates/low/src/a.rs", "mod b;\n"),
            ("crates/high/Cargo.toml", "[package]\nname = \"high\"\nversion = \"0.1.0\"\n\n[dependencies]\nlow.workspace = true\n"),
            ("crates/high/src/main.rs", "fn main() {}\n"),
            ("crates/side/Cargo.toml", "[package]\nname = \"side\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"tool\"\npath = \"src/tool.rs\"\n\n[[bin]]\nname = \"side\"\n"),
            ("crates/side/src/main.rs", "fn main() {}\n"),
            ("crates/side/README.md", "side\n"),
            ("crates/notes/README.md", "a folder the glob matches, without a manifest\n"),
        ]);
        let (summary, events) = run(&mut files, RustCascade::Module);
        let left: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            left,
            vec![
                "Cargo.toml",
                "crates/notes/README.md",
                "crates/side/Cargo.toml",
                "crates/side/README.md",
                "crates/side/src/main.rs"
            ],
            "{events:?}"
        );
        let root = String::from_utf8(files["Cargo.toml"].to_vec()).unwrap();
        assert!(root.contains("members = [\"crates/*\"]"), "{root}");
        assert!(root.contains("exclude = [\"crates/notes\"]"), "{root}");
        assert!(!root.contains("low = "), "{root}");
        let side = String::from_utf8(files["crates/side/Cargo.toml"].to_vec()).unwrap();
        assert!(!side.contains("tool"), "{side}");
        assert!(side.contains("name = \"side\""), "{side}");
        assert_eq!(summary.crates_stripped, 2, "{events:?}");
        assert!(events
            .iter()
            .all(|e| !e.detail.contains("a.rs") && !e.detail.contains("/b")));
    }

    #[test]
    fn unlink_mode_removes_the_declaration_its_doc_and_uses() {
        let mut files = tree(&[
            ("Cargo.toml", "[package]\nname = \"one\"\nversion = \"0.1.0\"\n"),
            (
                "src/lib.rs",
                "//! crate docs\nmod kept;\n/// gone's docs\n#[cfg(feature = \"x\")]\npub mod gone;\npub use gone::Thing;\nuse self::gone as g;\nuse kept::Other;\nmod twice; mod other;\n",
            ),
            ("src/kept.rs", "pub struct Other;\n"),
            ("src/other.rs", ""),
        ]);
        let (summary, _) = run(&mut files, RustCascade::Unlink);
        // `mod twice; mod other;` shares a line: not unlinkable, so the file would be stripped; keep that apart.
        assert!(!files.contains_key("src/lib.rs"));
        assert_eq!(summary.declarations_unlinked, 0);

        let mut files = tree(&[
            ("Cargo.toml", "[package]\nname = \"one\"\nversion = \"0.1.0\"\n"),
            (
                "src/lib.rs",
                "//! crate docs\nmod kept;\n/// gone's docs\n#[cfg(feature = \"x\")]\npub mod gone;\npub use gone::Thing;\nuse self::gone as g;\nuse kept::Other;\n",
            ),
            ("src/kept.rs", "pub struct Other;\n"),
        ]);
        let (summary, _) = run(&mut files, RustCascade::Unlink);
        assert_eq!(
            String::from_utf8(files["src/lib.rs"].to_vec()).unwrap(),
            "//! crate docs\nmod kept;\nuse kept::Other;\n"
        );
        assert_eq!(summary.declarations_unlinked, 1);
    }

    #[test]
    fn the_root_package_is_never_stripped_and_join_stays_in_the_tree() {
        let mut files = tree(&[
            ("Cargo.toml", "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n[dependencies]\ngone = { path = \"gone\" }\n"),
            ("src/lib.rs", ""),
        ]);
        let (summary, _) = run(&mut files, RustCascade::Module);
        assert_eq!(summary.files_stripped, 0);
        assert_eq!(join("a/b", "../../c"), Some("c".to_string()));
        assert_eq!(join("a", "../../c"), None);
        assert_eq!(join("a", "/abs"), None);
    }

    #[test]
    fn auto_mode_unlinks_an_unnamed_module_and_strips_a_named_one() {
        let base = [
            ("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n"),
            (
                "a/Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
            ),
            (
                "a/src/main.rs",
                "mod quiet;\nmod loud;\nmod used;\n\nfn main() {\n    used::go();\n}\n",
            ),
            ("a/src/used.rs", "mod inner;\npub fn go() {}\n"),
            ("a/src/other.rs", "pub fn f() { crate::loud::x() }\n"),
        ];
        let mut files = tree(&base);
        let (summary, events) = run(&mut files, RustCascade::Auto);
        // `quiet` is named nowhere: unlinked. `loud` is named by a path elsewhere: main.rs is stripped, so the crate
        // (no library, no binary left) goes too. `inner` is named nowhere: unlinked first.
        assert!(!files.contains_key("a/src/main.rs"), "{events:?}");
        assert!(!files.contains_key("a/Cargo.toml"), "{events:?}");
        assert_eq!(summary.declarations_unlinked, 1, "{events:?}");

        // A `pub use` of the module re-exports what a binary calls: it names the module.
        let mut files = tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"one\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/lib.rs",
                "mod service;\nmod quiet;\npub use service::run;\n",
            ),
            ("src/main.rs", "fn main() { one::run() }\n"),
        ]);
        let (summary, events) = run(&mut files, RustCascade::Auto);
        assert!(!files.contains_key("src/lib.rs"), "{events:?}");
        assert_eq!(summary.declarations_unlinked, 0, "{events:?}");
        let mut files = tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"one\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "use crate::{quiet as _};\nmod kept;\n"),
            ("src/kept.rs", "mod quiet;\n"),
        ]);
        let (_, events) = run(&mut files, RustCascade::Auto);
        assert!(!files.contains_key("src/kept.rs"), "{events:?}");
    }

    #[test]
    fn a_crate_whose_library_was_stripped_goes_whole_with_its_binaries() {
        let mut files = tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n"),
            (
                "a/Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
            ),
            ("a/src/main.rs", "fn main() { a::run() }\n"),
        ]);
        let mut before: BTreeSet<String> = files.keys().cloned().collect();
        before.insert("a/src/lib.rs".to_string());
        let mut modes = BTreeMap::new();
        let mut events = Vec::new();
        let summary = cascade_rust(
            &mut files,
            &mut modes,
            &before,
            RustCascade::Auto,
            &mut events,
        );
        assert_eq!(summary.crates_stripped, 1, "{events:?}");
        assert!(events
            .iter()
            .any(|e| e.detail.contains("its library was stripped")));
        let root = String::from_utf8(files["Cargo.toml"].to_vec()).unwrap();
        assert!(!root.contains("\"a\""), "{root}");
    }

    #[test]
    fn a_stripped_crate_keeps_the_licence_beside_a_nested_package() {
        let mut files = tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"kit\"]\nexclude = [\"kit/patches\"]\n"),
            ("kit/Cargo.toml", "[package]\nname = \"kit\"\nversion = \"0.1.0\"\n\n[dependencies]\ngone = { path = \"../gone\" }\n"),
            ("kit/src/lib.rs", ""),
            ("kit/LICENSE", "kit's own\n"),
            ("kit/patches/up/LICENSE-MIT", "upstream\n"),
            ("kit/patches/up/up-0.1.0/Cargo.toml", "[package]\nname = \"up\"\nversion = \"0.1.0\"\n"),
            ("kit/patches/up/up-0.1.0/src/lib.rs", ""),
            ("kit/notes/LICENSE", "no package beside it\n"),
        ]);
        let (summary, events) = run(&mut files, RustCascade::Auto);
        assert_eq!(summary.crates_stripped, 1, "{events:?}");
        let left: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            left,
            vec![
                "Cargo.toml",
                "kit/LICENSE",
                "kit/patches/up/LICENSE-MIT",
                "kit/patches/up/up-0.1.0/Cargo.toml",
                "kit/patches/up/up-0.1.0/src/lib.rs"
            ],
            "{events:?}"
        );
    }
    // A `[workspace.dependencies]` entry removed with its crate leaves no `x.workspace = true` behind it: a
    // dev-dependency that inherits it is unlinked, and a crate that needs it is stripped whole.
    #[test]
    fn an_inherited_dependency_on_a_gone_crate_goes_with_it() {
        let mut files = tree(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"app\", \"user\", \"gone\"]\n\n[workspace.dependencies]\n\
                 gone = { path = \"gone\" }\nserde = \"1\"\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde.workspace = true\n\n\
                 [dev-dependencies]\ngone.workspace = true\n",
            ),
            ("app/src/main.rs", "fn main() {}\n"),
            (
                "user/Cargo.toml",
                "[package]\nname = \"user\"\nversion = \"0.1.0\"\n\n[dependencies]\ngone = { workspace = true }\n",
            ),
            ("user/src/lib.rs", "pub fn u() {}\n"),
        ]);
        let (summary, events) = run(&mut files, RustCascade::Auto);
        let app = String::from_utf8(files["app/Cargo.toml"].to_vec()).unwrap();
        assert!(!app.contains("gone"), "{app}");
        assert!(app.contains("serde.workspace = true"), "{app}");
        assert!(!files.contains_key("user/src/lib.rs"), "{events:?}");
        assert_eq!(summary.crates_stripped, 1, "{events:?}");
        let root = String::from_utf8(files["Cargo.toml"].to_vec()).unwrap();
        assert!(!root.contains("gone") && root.contains("serde"), "{root}");
    }
    // `#[cfg(test)]` code is never part of the build: a test module whose file is gone is unlinked in
    // "auto" mode even when the tests name it, and an include inside a test block of a file that is gone strips
    // nothing.
    #[test]
    fn a_test_module_and_a_test_include_never_strip_the_build() {
        let mut files = tree(&[
            ("Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n"),
            (
                "src/lib.rs",
                "pub mod real;\n\n#[cfg(test)]\nmod tests;\n\n#[cfg(test)]\nmod inline_tests {\n    \
                 const F: &str = include_str!(\"../fixtures/f.txt\");\n\n    #[test]\n    fn t() {\n        \
                 super::tests::x(F);\n    }\n}\n",
            ),
            ("src/real.rs", "pub fn real() {}\n"),
        ]);
        let mut modes = BTreeMap::new();
        let mut events = Vec::new();
        let mut before: BTreeSet<String> = files.keys().cloned().collect();
        before.insert("src/tests.rs".to_string());
        before.insert("fixtures/f.txt".to_string());
        let summary = cascade_rust(
            &mut files,
            &mut modes,
            &before,
            RustCascade::Auto,
            &mut events,
        );
        let lib = String::from_utf8(files["src/lib.rs"].to_vec()).unwrap();
        assert!(lib.contains("pub mod real;"), "{lib}\n{events:?}");
        assert!(!lib.contains("mod tests;"), "{lib}");
        assert_eq!(summary.declarations_unlinked, 1, "{events:?}");
        assert_eq!(summary.files_stripped, 0, "{events:?}");
    }

    // An include or a module file the origin never had (code behind a feature) is not the export's doing;
    // an include the export left out still strips its includer.
    #[test]
    fn an_include_the_origin_never_had_is_left_to_the_build() {
        let base = tree(&[
            ("Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n"),
            (
                "src/lib.rs",
                "pub mod pins;\n#[cfg(feature = \"parity\")]\nmod parity;\n#[cfg(feature = \"parity\")]\npub use parity::Dump;\n",
            ),
            (
                "src/pins.rs",
                "pub fn pins() -> &'static str {\n    include_str!(\"../reference/pins.json\")\n}\n",
            ),
        ]);
        let mut origin: BTreeSet<String> = base.keys().cloned().collect();
        for (had, stripped) in [(false, 0), (true, 1)] {
            if had {
                origin.insert("reference/pins.json".to_string());
            }
            let mut files = base.clone();
            let mut modes = BTreeMap::new();
            let mut events = Vec::new();
            let before: BTreeSet<String> = files.keys().cloned().collect();
            cascade_rust_in(
                &mut files,
                &mut modes,
                &before,
                Some(&origin),
                RustCascade::Module,
                &mut events,
            );
            assert_eq!(
                usize::from(!files.contains_key("src/pins.rs")),
                stripped,
                "origin had the file: {had}; {events:?}"
            );
            if !had {
                // The feature's module was never in the origin: its declaration stays, and so does the library.
                assert!(files.contains_key("src/lib.rs"), "{events:?}");
            }
        }
    }
}
