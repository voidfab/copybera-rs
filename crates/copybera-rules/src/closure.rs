//! `copybera closure`: the source closure of named binaries, as a workflow's `[paths.origin] include`.
//!
//! An export of "exactly what these apps are built from" starts from the apps' package folders and follows what cargo
//! (and a JavaScript workspace) would read to build them:
//!
//! - each package's normal and build path dependencies (`[dependencies]`, `[build-dependencies]` and their
//!   `[target.*]` forms; `[dev-dependencies]` only on request), a `workspace = true` one through its workspace's
//!   `[workspace.dependencies]`;
//! - each package's workspace root (cargo's rule: the nearest folder above with a `[workspace]` that does not exclude
//!   it, else the package itself, a detached workspace): its `Cargo.toml` and `Cargo.lock`, the root's `[patch.*]` and
//!   `[replace]` path entries (cargo loads them whether or not they are used), and the cargo config and toolchain files
//!   of that folder and every folder above it;
//! - files outside a package that its build sources name by a relative path: an `include_str!` or `#[path]` target,
//!   a build script's input, a `concat!(env!("CARGO_MANIFEST_DIR"), "/../..")` path. A string literal holding `../`
//!   is read both from the file's folder and from the package folder, and kept when it names a file or folder of the
//!   tree outside the package (never a folder above the package). Test sources (`tests/`, `benches/`, `examples/`,
//!   `*_tests.rs`, `tests.rs`) are read only with dev-dependencies;
//! - for a JavaScript package (`--node`): its `workspace:`, `file:` and `link:` dependencies, resolved through the
//!   nearest `package.json` above it with `workspaces`, that root's manifest, lock and config files, and the relative
//!   imports of its sources that leave the package.
//!
//! The pass is pure: it reads the tree's paths and the bytes of the text files it needs. Each include entry records
//! the first reason it was added.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use globset::{Glob, GlobSetBuilder};
use regex::Regex;

use crate::transform::Blob;
use crate::workflow::PathFilter;

/// What a closure starts from.
#[derive(Debug, Clone, Default)]
pub struct ClosureRequest {
    /// Cargo package folders (each with a `Cargo.toml`), relative to the tree's root.
    pub cargo: Vec<String>,
    /// JavaScript package folders (each with a `package.json`).
    pub node: Vec<String>,
    /// Follow `[dev-dependencies]` (and `devDependencies` in the workspace) and read test sources too.
    pub dev: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Closure {
    /// Include entry (`dir/**` for a package or a named folder, a path for a file) -> the first reason it was added.
    pub entries: BTreeMap<String, String>,
    pub crates: BTreeSet<String>,
    pub workspaces: BTreeSet<String>,
    pub node_packages: BTreeSet<String>,
}

impl Closure {
    /// The include entries, minus any that an included folder already covers.
    pub fn include(&self) -> Vec<String> {
        let folders: Vec<&str> = self
            .entries
            .keys()
            .filter_map(|e| e.strip_suffix("/**"))
            .collect();
        self.entries
            .keys()
            .filter(|entry| {
                let path = entry.strip_suffix("/**").unwrap_or(entry);
                !folders.iter().any(|f| *f != path && under(path, f))
            })
            .cloned()
            .collect()
    }

    /// `include = [ .. ]` as TOML, one entry a line.
    pub fn include_toml(&self) -> String {
        let mut out = String::from("include = [\n");
        for entry in self.include() {
            out.push_str(&format!("  {},\n", toml_string(&entry)));
        }
        out.push_str("]\n");
        out
    }

    /// Every tree path an include entry names.
    pub fn paths_of<'a>(entry: &str, paths: &'a BTreeSet<String>) -> Vec<&'a String> {
        match entry.strip_suffix("/**") {
            Some(dir) => paths.iter().filter(|p| under(p, dir)).collect(),
            None => paths.iter().filter(|p| p.as_str() == entry).collect(),
        }
    }
}

fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The text files a closure reads: manifests, Rust sources, JavaScript and TypeScript sources and their configs.
pub fn source_filter() -> PathFilter {
    let include: Vec<String> = [
        "**/Cargo.toml",
        "**/*.rs",
        "**/package.json",
        "**/tsconfig*.json",
        "**/*.ts",
        "**/*.tsx",
        "**/*.js",
        "**/*.mjs",
        "**/*.cjs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let exclude = vec!["**/node_modules/**".to_string()];
    let build = |globs: &[String]| {
        let mut builder = GlobSetBuilder::new();
        for g in globs {
            builder.add(Glob::new(g).expect("the closure's source globs compile"));
        }
        builder.build().expect("the closure's source globs build")
    };
    PathFilter {
        include_set: build(&include),
        exclude_set: build(&exclude),
        include,
        exclude,
    }
}

const CARGO_CONFIG: [&str; 4] = [
    ".cargo/config.toml",
    ".cargo/config",
    "rust-toolchain.toml",
    "rust-toolchain",
];
const NODE_ROOT_FILES: [&str; 9] = [
    "package.json",
    "bun.lock",
    "bun.lockb",
    "bunfig.toml",
    "package-lock.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "yarn.lock",
    ".npmrc",
];
const NODE_DEP_TABLES: [&str; 4] = [
    "dependencies",
    "optionalDependencies",
    "peerDependencies",
    "devDependencies",
];
const NODE_EXTENSIONS: [&str; 8] = [
    ".ts", ".tsx", ".js", ".mjs", ".cjs", ".json", ".css", ".d.ts",
];

struct Walk<'a> {
    paths: &'a BTreeSet<String>,
    files: &'a BTreeMap<String, Blob>,
    dev: bool,
    out: Closure,
    literal: Regex,
}

/// The closure of `request` over a tree: `paths` is every path of the tree, `files` the bytes of the files
/// `source_filter` names (a path missing there is read as empty).
pub fn closure(
    paths: &BTreeSet<String>,
    files: &BTreeMap<String, Blob>,
    request: &ClosureRequest,
) -> Result<Closure, String> {
    let mut walk = Walk {
        paths,
        files,
        dev: request.dev,
        out: Closure::default(),
        literal: Regex::new(r#""((?:[^"\\\n]|\\.)*)""#).expect("the literal pattern compiles"),
    };
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    for root in &request.cargo {
        let root =
            normalize(root).ok_or_else(|| format!("--cargo {root}: not a folder of the tree"))?;
        queue.push_back((root, "named".to_string()));
    }
    while let Some((dir, cause)) = queue.pop_front() {
        walk.cargo_package(&dir, &cause, &mut queue)?;
    }
    let mut node_queue: VecDeque<(String, String)> = VecDeque::new();
    for root in &request.node {
        let root =
            normalize(root).ok_or_else(|| format!("--node {root}: not a folder of the tree"))?;
        node_queue.push_back((root, "named".to_string()));
    }
    while let Some((dir, cause)) = node_queue.pop_front() {
        walk.node_package(&dir, &cause, &mut node_queue)?;
    }
    Ok(walk.out)
}

fn normalize(dir: &str) -> Option<String> {
    let joined = join("", dir.trim_end_matches('/'))?;
    (!joined.is_empty()).then_some(joined)
}

impl Walk<'_> {
    fn add(&mut self, entry: String, cause: impl Into<String>) {
        self.out
            .entries
            .entry(entry)
            .or_insert_with(|| cause.into());
    }

    fn text(&self, path: &str) -> &str {
        self.files
            .get(path)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    fn is_dir(&self, dir: &str) -> bool {
        !dir.is_empty() && self.paths.iter().any(|p| under(p, dir))
    }

    fn manifest(&self, dir: &str) -> Option<toml::Table> {
        let path = in_dir(dir, "Cargo.toml");
        if !self.paths.contains(&path) {
            return None;
        }
        self.text(&path).parse::<toml::Table>().ok()
    }

    /// The workspace root a package belongs to, by cargo's rule.
    fn workspace_of(&self, dir: &str, table: &toml::Table) -> String {
        if table.contains_key("workspace") {
            return dir.to_string();
        }
        let mut up = dir.to_string();
        while !up.is_empty() {
            up = dir_of(&up).to_string();
            let Some(ws) = self.manifest(&up) else {
                continue;
            };
            let Some(spec) = ws.get("workspace").and_then(|w| w.as_table()) else {
                continue;
            };
            let excluded = str_array(spec.get("exclude"))
                .iter()
                .filter_map(|e| join(&up, e))
                .any(|e| e == dir || under(dir, &e));
            if !excluded {
                return up;
            }
            break;
        }
        dir.to_string()
    }

    fn cargo_package(
        &mut self,
        dir: &str,
        cause: &str,
        queue: &mut VecDeque<(String, String)>,
    ) -> Result<(), String> {
        if self.out.crates.contains(dir) {
            return Ok(());
        }
        let table = self
            .manifest(dir)
            .ok_or_else(|| format!("{dir}: no readable Cargo.toml ({cause})"))?;
        self.out.crates.insert(dir.to_string());
        self.add(format!("{dir}/**"), format!("package ({cause})"));
        let ws = self.workspace_of(dir, &table);
        let ws_table = if ws == dir {
            table.clone()
        } else {
            self.manifest(&ws).unwrap_or_default()
        };
        if self.out.workspaces.insert(ws.clone()) {
            self.workspace_root(&ws, &ws_table, queue);
        }
        for (kind, name, spec) in dependency_specs(&table) {
            if kind == "dev-dependencies" && !self.dev {
                continue;
            }
            let target = if spec.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
                ws_table
                    .get("workspace")
                    .and_then(|w| w.get("dependencies"))
                    .and_then(|d| d.get(&name))
                    .and_then(path_of)
                    .and_then(|p| join(&ws, &p))
            } else {
                path_of(&spec).and_then(|p| join(dir, &p))
            };
            if let Some(target) = target {
                queue.push_back((target, format!("{kind} of {dir}")));
            }
        }
        // Manifest-named files outside the package folder (a `[lib]` or target path, a build script).
        let mut named: Vec<String> = Vec::new();
        if let Some(build) = table
            .get("package")
            .and_then(|p| p.get("build"))
            .and_then(|b| b.as_str())
        {
            named.push(build.to_string());
        }
        if let Some(p) = table.get("lib").and_then(path_of) {
            named.push(p);
        }
        for kind in ["bin", "test", "example", "bench"] {
            if kind != "bin" && !self.dev {
                continue;
            }
            for target in table
                .get(kind)
                .and_then(|t| t.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(p) = path_of(target) {
                    named.push(p);
                }
            }
        }
        for rel in named {
            if let Some(path) = join(dir, &rel) {
                if !under(&path, dir) && self.paths.contains(&path) {
                    self.add(path, format!("named by {dir}/Cargo.toml"));
                }
            }
        }
        self.scan_sources(dir, &[".rs"], true);
        Ok(())
    }

    fn workspace_root(
        &mut self,
        ws: &str,
        table: &toml::Table,
        queue: &mut VecDeque<(String, String)>,
    ) {
        for name in ["Cargo.toml", "Cargo.lock"] {
            let path = in_dir(ws, name);
            if self.paths.contains(&path) {
                self.add(path, format!("workspace root {}", shown(ws)));
            }
        }
        let mut up = Some(ws.to_string());
        while let Some(dir) = up {
            for name in CARGO_CONFIG {
                let path = in_dir(&dir, name);
                if self.paths.contains(&path) {
                    self.add(path, format!("cargo config above {}", shown(ws)));
                }
            }
            up = (!dir.is_empty()).then(|| dir_of(&dir).to_string());
        }
        let mut patches: Vec<(String, String)> = Vec::new();
        if let Some(sources) = table.get("patch").and_then(|p| p.as_table()) {
            for (source, crates) in sources {
                for (name, spec) in crates.as_table().into_iter().flatten() {
                    if let Some(p) = path_of(spec) {
                        patches.push((p, format!("[patch.{source}] {name} of {}", shown(ws))));
                    }
                }
            }
        }
        for (name, spec) in table
            .get("replace")
            .and_then(|r| r.as_table())
            .into_iter()
            .flatten()
        {
            if let Some(p) = path_of(spec) {
                patches.push((p, format!("[replace] {name} of {}", shown(ws))));
            }
        }
        for (rel, cause) in patches {
            if let Some(dir) = join(ws, &rel) {
                queue.push_back((dir, cause));
            }
        }
    }

    /// Relative paths the package's own sources name outside the package folder.
    fn scan_sources(&mut self, dir: &str, extensions: &[&str], from_package_dir: bool) {
        let nested: Vec<String> = self
            .paths
            .iter()
            .filter(|p| {
                (p.ends_with("/Cargo.toml") || p.ends_with("/package.json"))
                    && under(p, dir)
                    && dir_of(p) != dir
            })
            .map(|p| dir_of(p).to_string())
            .collect();
        let sources: Vec<String> = self
            .paths
            .iter()
            .filter(|p| under(p, dir) && !nested.iter().any(|n| under(p, n)))
            .filter(|p| extensions.iter().any(|e| p.ends_with(e)))
            .filter(|p| !p.contains("/node_modules/"))
            .filter(|p| self.dev || !is_test_source(rel_to(p, dir).unwrap_or(p)))
            .cloned()
            .collect();
        let mut found: Vec<(String, String)> = Vec::new();
        for source in &sources {
            let text = self.text(source);
            for line in text.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                for caps in self.literal.captures_iter(line) {
                    let lit = &caps[1];
                    if !lit.contains("../") {
                        continue;
                    }
                    let mut candidates = vec![join(dir_of(source), lit)];
                    if from_package_dir {
                        candidates.push(join(dir, lit.trim_start_matches('/')));
                    }
                    for candidate in candidates.into_iter().flatten() {
                        if let Some(entry) = self.entry_for(&candidate, dir, !from_package_dir) {
                            found.push((entry, format!("named by {source}")));
                        }
                    }
                }
            }
        }
        for (entry, cause) in found {
            self.add(entry, cause);
        }
    }

    /// The include entry for a path a source names: a file, a folder (`dir/**`), or for an import a file with one
    /// of the module extensions; `None` inside the package, above it, or not in the tree.
    fn entry_for(&self, candidate: &str, package: &str, module: bool) -> Option<String> {
        if candidate.is_empty() || under(candidate, package) || candidate == package {
            return None;
        }
        if package.is_empty() || under(package, candidate) {
            return None;
        }
        if self.paths.contains(candidate) {
            return Some(candidate.to_string());
        }
        if self.is_dir(candidate) {
            return Some(format!("{candidate}/**"));
        }
        if module {
            for ext in NODE_EXTENSIONS {
                let path = format!("{candidate}{ext}");
                if self.paths.contains(&path) {
                    return Some(path);
                }
            }
        }
        None
    }

    fn node_package(
        &mut self,
        dir: &str,
        cause: &str,
        queue: &mut VecDeque<(String, String)>,
    ) -> Result<(), String> {
        if self.out.node_packages.contains(dir) {
            return Ok(());
        }
        let manifest_path = in_dir(dir, "package.json");
        let manifest: serde_json::Value = serde_json::from_str(self.text(&manifest_path))
            .map_err(|_| format!("{dir}: no readable package.json ({cause})"))?;
        self.out.node_packages.insert(dir.to_string());
        self.add(format!("{dir}/**"), format!("package ({cause})"));
        let root = self.node_workspace_of(dir);
        if let Some(root) = &root {
            for name in NODE_ROOT_FILES {
                let path = in_dir(root, name);
                if self.paths.contains(&path) {
                    self.add(path, format!("JavaScript workspace root {}", shown(root)));
                }
            }
        }
        let members = root
            .as_deref()
            .map(|r| self.node_members(r))
            .unwrap_or_default();
        for table in NODE_DEP_TABLES {
            if table == "devDependencies" && !self.dev && cause != "named" {
                // A workspace package's dev tools are not what the app is built from; the named package's are.
                continue;
            }
            let Some(deps) = manifest.get(table).and_then(|d| d.as_object()) else {
                continue;
            };
            for (name, version) in deps {
                let Some(version) = version.as_str() else {
                    continue;
                };
                let target = if version.starts_with("workspace:") {
                    members.get(name).cloned()
                } else if let Some(rel) = version
                    .strip_prefix("file:")
                    .or_else(|| version.strip_prefix("link:"))
                {
                    join(dir, rel)
                } else {
                    None
                };
                if let Some(target) = target {
                    queue.push_back((target, format!("{table} of {dir}")));
                }
            }
        }
        self.scan_sources(dir, &[".ts", ".tsx", ".js", ".mjs", ".cjs", ".json"], false);
        Ok(())
    }

    fn node_workspace_of(&self, dir: &str) -> Option<String> {
        let mut up = Some(dir.to_string());
        while let Some(d) = up {
            let path = in_dir(&d, "package.json");
            if self.paths.contains(&path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(self.text(&path)) {
                    if v.get("workspaces").is_some() {
                        return Some(d);
                    }
                }
            }
            up = (!d.is_empty()).then(|| dir_of(&d).to_string());
        }
        None
    }

    /// Package name -> folder, for the workspace's member globs (a `!` glob removes).
    fn node_members(&self, root: &str) -> BTreeMap<String, String> {
        let path = in_dir(root, "package.json");
        let Ok(v) = serde_json::from_str::<serde_json::Value>(self.text(&path)) else {
            return BTreeMap::new();
        };
        let globs: Vec<String> = match v.get("workspaces") {
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|g| g.as_str())
                .map(str::to_string)
                .collect(),
            Some(serde_json::Value::Object(o)) => o
                .get("packages")
                .and_then(|p| p.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|g| g.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let (mut add, mut remove) = (GlobSetBuilder::new(), GlobSetBuilder::new());
        for g in &globs {
            let (set, g) = match g.strip_prefix('!') {
                Some(g) => (&mut remove, g),
                None => (&mut add, g.as_str()),
            };
            if let Ok(glob) = globset::GlobBuilder::new(&in_dir(root, g.trim_end_matches('/')))
                .literal_separator(true)
                .build()
            {
                set.add(glob);
            }
        }
        let (Ok(add), Ok(remove)) = (add.build(), remove.build()) else {
            return BTreeMap::new();
        };
        let mut members = BTreeMap::new();
        for p in self.paths.iter().filter(|p| p.ends_with("/package.json")) {
            let d = dir_of(p);
            if p.contains("/node_modules/") || !add.is_match(d) || remove.is_match(d) {
                continue;
            }
            if let Some(name) = serde_json::from_str::<serde_json::Value>(self.text(p))
                .ok()
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            {
                members.entry(name).or_insert_with(|| d.to_string());
            }
        }
        members
    }
}

fn shown(dir: &str) -> &str {
    if dir.is_empty() {
        "(the tree's root)"
    } else {
        dir
    }
}

fn is_test_source(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    let name = file_name(rel);
    matches!(
        first,
        "tests" | "benches" | "examples" | "test" | "__tests__"
    ) || name == "tests.rs"
        || name.ends_with("_tests.rs")
        || name.ends_with("_test.rs")
        || name.contains(".test.")
        || name.contains(".spec.")
}

const DEP_TABLES: [&str; 3] = ["dependencies", "build-dependencies", "dev-dependencies"];

fn dependency_specs(table: &toml::Table) -> Vec<(&'static str, String, toml::Value)> {
    let mut out = Vec::new();
    let mut walk = |t: &toml::Table| {
        for kind in DEP_TABLES {
            for (name, spec) in t.get(kind).and_then(|d| d.as_table()).into_iter().flatten() {
                out.push((kind, name.clone(), spec.clone()));
            }
        }
    };
    walk(table);
    for (_, t) in table
        .get("target")
        .and_then(|t| t.as_table())
        .into_iter()
        .flatten()
    {
        if let Some(t) = t.as_table() {
            walk(t);
        }
    }
    out
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

fn rel_to<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        Some(path)
    } else {
        path.strip_prefix(dir)?.strip_prefix('/')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(entries: &[(&str, &str)]) -> (BTreeSet<String>, BTreeMap<String, Blob>) {
        let paths = entries.iter().map(|(p, _)| p.to_string()).collect();
        let files = entries
            .iter()
            .map(|(p, b)| (p.to_string(), Blob::from(b.as_bytes())))
            .collect();
        (paths, files)
    }

    fn pkg(name: &str, extra: &str) -> String {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{extra}")
    }

    #[test]
    fn follows_workspace_dependencies_and_patches_but_not_dev_dependencies() {
        let root = "[workspace]\nmembers = [\"a\", \"b\", \"t\"]\n[workspace.dependencies]\nb = { path = \"b\" }\n\
                    [patch.crates-io]\nfoo = { path = \"vendor/foo\" }\n";
        let a = pkg(
            "a",
            "[dependencies]\nb.workspace = true\n[dev-dependencies]\nt = { path = \"../t\" }\n",
        );
        let b = pkg("b", "");
        let t = pkg("t", "");
        let foo = pkg("foo", "");
        let (paths, files) = tree(&[
            ("Cargo.toml", root),
            ("Cargo.lock", ""),
            ("a/Cargo.toml", &a),
            ("a/src/main.rs", "fn main() {}\n"),
            ("b/Cargo.toml", &b),
            ("b/src/lib.rs", ""),
            ("t/Cargo.toml", &t),
            ("t/src/lib.rs", ""),
            ("vendor/foo/Cargo.toml", &foo),
            ("vendor/foo/src/lib.rs", ""),
        ]);
        let request = ClosureRequest {
            cargo: vec!["a".into()],
            ..Default::default()
        };
        let c = closure(&paths, &files, &request).unwrap();
        assert_eq!(
            c.include(),
            vec!["Cargo.lock", "Cargo.toml", "a/**", "b/**", "vendor/foo/**"]
        );
        let request = ClosureRequest {
            dev: true,
            ..request
        };
        let c = closure(&paths, &files, &request).unwrap();
        assert!(c.include().contains(&"t/**".to_string()));
    }

    #[test]
    fn reads_relative_literals_outside_the_package_and_never_above_it() {
        let a = pkg("a", "");
        let (paths, files) = tree(&[
            ("crates/a/Cargo.toml", &a),
            (
                "crates/a/src/lib.rs",
                "const X: &str = include_str!(\"../../../data/x.txt\");\n\
                 const Y: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../catalog\"));\n\
                 // \"../../docs/a.md\" is a comment\n\
                 const UP: &str = \"..\";\nconst ROOT: &str = \"../../\";\n",
            ),
            ("crates/a/tests/t.rs", "const F: &str = include_str!(\"../../../fixtures/f.txt\");\n"),
            ("data/x.txt", "x"),
            ("catalog/m.toml", "m"),
            ("docs/a.md", "a"),
            ("fixtures/f.txt", "f"),
        ]);
        let request = ClosureRequest {
            cargo: vec!["crates/a".into()],
            ..Default::default()
        };
        let c = closure(&paths, &files, &request).unwrap();
        assert_eq!(c.include(), vec!["catalog/**", "crates/a/**", "data/x.txt"]);
    }

    #[test]
    fn a_detached_package_is_its_own_workspace() {
        let root = "[workspace]\nmembers = [\"a\"]\nexclude = [\"apps/d\"]\n";
        let (paths, files) = tree(&[
            ("Cargo.toml", root),
            ("a/Cargo.toml", &pkg("a", "")),
            ("a/src/lib.rs", ""),
            (
                "apps/d/Cargo.toml",
                &pkg(
                    "d",
                    "[workspace]\n[dependencies]\na = { path = \"../../a\" }\n",
                ),
            ),
            ("apps/d/src/main.rs", ""),
            ("apps/e/Cargo.toml", &pkg("e", "")),
            ("apps/e/src/main.rs", ""),
        ]);
        let request = ClosureRequest {
            cargo: vec!["apps/d".into(), "apps/e".into()],
            ..Default::default()
        };
        let c = closure(&paths, &files, &request).unwrap();
        // apps/e is not excluded, so the root is its workspace; apps/d is detached.
        assert_eq!(
            c.workspaces.iter().cloned().collect::<Vec<_>>(),
            vec!["".to_string(), "apps/d".to_string()]
        );
        assert!(closure(
            &paths,
            &files,
            &ClosureRequest {
                cargo: vec!["nope".into()],
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn node_workspace_dependencies_resolve_by_name_through_the_workspace_globs() {
        let (paths, files) = tree(&[
            (
                "package.json",
                r#"{"workspaces": ["packages/*", "apps/*", "!apps/skip"]}"#,
            ),
            ("bun.lock", ""),
            (
                "apps/web/package.json",
                r#"{"name": "web", "dependencies": {"@x/ui": "workspace:*", "react": "^19"}, "devDependencies": {"vite": "^7"}}"#,
            ),
            (
                "apps/web/src/main.ts",
                "import { a } from \"../../../shared/a\";\nimport x from \"@x/ui\";\n",
            ),
            (
                "packages/ui/package.json",
                r#"{"name": "@x/ui", "devDependencies": {"@x/tool": "workspace:*"}}"#,
            ),
            ("packages/ui/src/index.ts", ""),
            ("packages/tool/package.json", r#"{"name": "@x/tool"}"#),
            ("apps/skip/package.json", r#"{"name": "@x/ui"}"#),
            ("shared/a.ts", "export const a = 1;\n"),
        ]);
        let request = ClosureRequest {
            node: vec!["apps/web".into()],
            ..Default::default()
        };
        let c = closure(&paths, &files, &request).unwrap();
        assert_eq!(
            c.include(),
            vec![
                "apps/web/**",
                "bun.lock",
                "package.json",
                "packages/ui/**",
                "shared/a.ts"
            ]
        );
    }
}
