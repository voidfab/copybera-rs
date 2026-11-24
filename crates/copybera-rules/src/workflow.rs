use globset::GlobSet;
use regex::Regex;
use toml::Value;

use crate::error::ConfigError;
use crate::globmatch::globset;

const TOP_KEYS: &[&str] = &[
    "workflow",
    "origin",
    "destination",
    "paths",
    "authoring",
    "transformations",
    "lint",
    "scrub",
    "gate",
    "cascade",
    "verify",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Iterative,
    Squash,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Iterative => "iterative",
            Mode::Squash => "squash",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Env { name: String },
    Helper { name: String },
    InsteadOf { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub location: Location,
    pub ref_name: String,
    pub since: Option<String>,
    /// `subjects = "clean"`: migrate refuses, before it walks, a range whose commit subjects match a private
    /// pattern (a squash message carries every subject), and names the commit to start after.
    pub subjects_clean: bool,
    pub auth: Option<Auth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub location: Location,
    pub fetch: String,
    pub push: String,
    pub auth: Option<Auth>,
}

#[derive(Debug, Clone)]
pub struct PathFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_set: GlobSet,
    pub exclude_set: GlobSet,
}

impl PathFilter {
    pub fn allows(&self, path: &str) -> bool {
        crate::globmatch::path_allowed(
            path,
            &self.include_set,
            &self.exclude_set,
            self.include.is_empty(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorMap {
    pub from: Identity,
    pub to: Identity,
}

#[derive(Debug, Clone)]
pub struct LintAllow {
    pub paths: Vec<String>,
    pub patterns: Vec<usize>,
    pub matcher: GlobSet,
}

impl LintAllow {
    pub fn from_parts(paths: Vec<String>, patterns: Vec<usize>) -> Result<Self, ConfigError> {
        let matcher = globset(&paths, "lint.allow.paths")?;
        Ok(Self {
            paths,
            patterns,
            matcher,
        })
    }

    pub fn allows(&self, path: &str, pattern: usize) -> bool {
        self.patterns.contains(&pattern) && self.matcher.is_match(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authoring {
    pub default: Identity,
    pub map: Vec<AuthorMap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceOn {
    Files,
    Messages,
    Both,
}

/// Which files a text transformation touches and which lines it leaves verbatim. `paths` (default: every file) and `exclude` are globs over destination paths; a message is
/// not a path, so only `on` decides whether a message is rewritten. A line matching any `ignore_lines` regex is left
/// as it was, in files and messages; with `ignore_lines` set, the transformation's pattern is matched line by line.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub paths: Vec<String>,
    pub exclude: Vec<String>,
    pub ignore_lines: Vec<String>,
    paths_set: Option<GlobSet>,
    exclude_set: Option<GlobSet>,
    ignore_regexes: Vec<Regex>,
}

impl Scope {
    /// True when the transformation applies to the file at `path`.
    pub fn covers(&self, path: &str) -> bool {
        self.paths_set.as_ref().is_none_or(|set| set.is_match(path))
            && !self
                .exclude_set
                .as_ref()
                .is_some_and(|set| set.is_match(path))
    }

    /// True when `line` (without its line ending) is left verbatim.
    pub fn ignores(&self, line: &str) -> bool {
        self.ignore_regexes.iter().any(|regex| regex.is_match(line))
    }

    pub fn has_ignore_lines(&self) -> bool {
        !self.ignore_regexes.is_empty()
    }
}

#[derive(Debug, Clone)]
pub enum Transformation {
    StripPaths {
        globs: Vec<String>,
        matcher: GlobSet,
    },
    StripReferences {
        patterns: Vec<String>,
        regexes: Vec<Regex>,
    },
    RewriteMessages {
        hook: Option<String>,
        prompt_budget_tokens: u32,
        fallback: String,
    },
    ReplaceText {
        pattern: String,
        replacement: String,
        on: ReplaceOn,
        regex: Regex,
        scope: Scope,
    },
    /// Ordered literal pairs with copybara's sentinel semantics: a pair never matches text an earlier pair wrote, and
    /// the scope's ignore_lines are judged on the line as it came in.
    ReplacePairs {
        pairs: Vec<(String, String)>,
        on: ReplaceOn,
        scope: Scope,
        /// Put the name pairs of every earlier move_paths in this run ahead of `pairs`, longest first.
        follow_moves: bool,
    },
    /// A covered UTF-8 file whose text matches any pattern is stripped (a file such as a dated record is
    /// stripped rather than scrubbed when its wording must be kept as it was). Events name the pattern's number only.
    StripMatching {
        patterns: Vec<String>,
        regexes: Vec<Regex>,
        scope: Scope,
        /// `private = true`: also the private-patterns file's patterns, matched line by line as the lint does; the CLI
        /// loads them into `private_regexes` before the run (fail closed when the file cannot be read).
        private: bool,
        private_regexes: Vec<Regex>,
    },
    /// A covered path whose segments carry a pair moves to its new name, unless that name is taken by another path
    /// or by a second move.
    MovePaths {
        pairs: Vec<(String, String)>,
        scope: Scope,
    },
    RenamePaths {
        from: String,
        to: String,
    },
    /// Source comments removed per language: off unless listed. `keep_docs` keeps doc comments.
    StripComments {
        languages: Vec<crate::comments::Lang>,
        keep_docs: bool,
        scope: Scope,
    },
    DropEmptyCommits,
}

impl Transformation {
    pub fn type_name(&self) -> &'static str {
        match self {
            Transformation::StripPaths { .. } => "strip_paths",
            Transformation::StripReferences { .. } => "strip_references",
            Transformation::RewriteMessages { .. } => "rewrite_messages",
            Transformation::ReplaceText { .. } => "replace_text",
            Transformation::ReplacePairs { .. } => "replace_pairs",
            Transformation::MovePaths { .. } => "move_paths",
            Transformation::StripMatching { .. } => "strip_matching",
            Transformation::RenamePaths { .. } => "rename_paths",
            Transformation::StripComments { .. } => "strip_comments",
            Transformation::DropEmptyCommits => "drop_empty_commits",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: String,
    pub mode: Mode,
    pub dry_run: bool,
    pub receipt: Option<String>,
    pub origin: Origin,
    pub destination: Destination,
    pub origin_paths: PathFilter,
    pub destination_paths: PathFilter,
    pub authoring: Authoring,
    pub transformations: Vec<Transformation>,
    /// `[lint] private`: the file of private patterns (one regex per line) the lint refuses in the destination. The
    /// file lives outside git; a relative path is read from the workflow file's folder.
    pub lint_private: Option<String>,
    /// `[[lint.allow]]`: private-pattern numbers exempted for matching destination paths.
    pub lint_allow: Vec<LintAllow>,
    /// `[lint] references = "keep"`: the export keeps reference ids in text on purpose (strip_references off), so the
    /// lint does not refuse them in messages or file contents; a path that matches one still refuses.
    pub lint_keep_references: bool,
    /// `[lint] ids`: the reference patterns (regexes) the lint refuses in messages, file contents and paths. When
    /// absent, the lint refuses what the workflow's `strip_references` transformations strip (see
    /// [`Workflow::lint_references`]).
    pub lint_ids: Option<Vec<String>>,
    lint_id_regexes: Vec<Regex>,
    /// `[lint] private_action = "strip"`: every file the private patterns hit in the tree as
    /// written (after every transformation and the private scrub), by name or by a line of text, is dropped whole
    /// instead of refusing the run; the lint's allowances do not spare it. `"refuse"` (the default) leaves it to the lint.
    pub lint_private_strip: bool,
    /// `[scrub] private`: regex and replacement rules applied after all workflow transformations.
    pub scrub_private: Option<String>,
    /// `[gate]`: what `copybera gate` judges the exported tree against.
    pub gate: Option<crate::gate::GateConfig>,
    /// `[cascade] rust`: after every transformation, the private scrub and the private strip, repair or
    /// strip what the stripped files broke in the tree's Rust crates (see `rust_cascade`).
    pub cascade_rust: Option<crate::rust_cascade::RustCascade>,
    /// `[verify]`: commands run in the tree as it will be written, before anything is written.
    pub verify: Option<Verify>,
}

/// `[verify]`: each command runs in a folder holding the tree as it will be written (the last commit's tree), in
/// order, before the destination is written; the first failure refuses the run unless `on_fail = "report"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verify {
    /// Each command and its arguments (no shell).
    pub commands: Vec<Vec<String>>,
    /// Environment set for every command.
    pub env: Vec<(String, String)>,
    /// Where the tree is written (relative to the workflow file's folder; it must be absent or empty, and it is
    /// kept). Without it, a temporary folder is used and removed.
    pub dir: Option<String>,
    /// `on_fail = "refuse"` (the default): a failing command refuses the run and nothing is written.
    pub refuse: bool,
}

impl Workflow {
    /// True when a transformation reads the private-patterns file (`strip_matching` with `private = true`).
    pub fn needs_private_patterns(&self) -> bool {
        self.transformations
            .iter()
            .any(|t| matches!(t, Transformation::StripMatching { private: true, .. }))
    }

    /// Hands the private-patterns file's regexes to every `strip_matching` with `private = true`.
    pub fn set_private_patterns(&mut self, regexes: &[Regex]) {
        for transform in &mut self.transformations {
            if let Transformation::StripMatching {
                private: true,
                private_regexes,
                ..
            } = transform
            {
                *private_regexes = regexes.to_vec();
            }
        }
    }

    /// The reference patterns the lint refuses: `[lint] ids` when set, else every `strip_references` pattern.
    pub fn lint_references(&self) -> (Vec<String>, Vec<Regex>) {
        match &self.lint_ids {
            Some(ids) => (ids.clone(), self.lint_id_regexes.clone()),
            None => {
                let patterns = self
                    .transformations
                    .iter()
                    .filter_map(|t| match t {
                        Transformation::StripReferences { patterns, .. } => Some(patterns.clone()),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                (patterns, self.reference_regexes())
            }
        }
    }

    pub fn reference_regexes(&self) -> Vec<Regex> {
        self.transformations
            .iter()
            .filter_map(|t| match t {
                Transformation::StripReferences { regexes, .. } => Some(regexes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }
}

/// Parse a workflow TOML document. Every refusal names the key that failed.
pub fn parse_workflow(text: &str) -> Result<Workflow, ConfigError> {
    let value: Value = text
        .parse()
        .map_err(|err| ConfigError::new("document", format!("TOML parse error: {err}")))?;
    let root = value
        .as_table()
        .ok_or_else(|| ConfigError::new("document", "must be a table"))?;
    refuse_unknown_keys(root, TOP_KEYS, "document")?;

    let workflow_tbl = req_table(root, "workflow", "workflow")?;
    refuse_unknown_keys(
        workflow_tbl,
        &["name", "mode", "dry_run", "receipt"],
        "workflow",
    )?;
    let name = req_str(workflow_tbl, "name", "workflow")?;
    let mode = match req_str(workflow_tbl, "mode", "workflow")?.as_str() {
        "iterative" => Mode::Iterative,
        "squash" => Mode::Squash,
        _ => {
            return Err(ConfigError::new(
                "workflow.mode",
                "must be \"iterative\" or \"squash\"",
            ))
        }
    };
    let dry_run = opt_bool(workflow_tbl, "dry_run", "workflow")?.unwrap_or(false);
    let receipt = opt_str(workflow_tbl, "receipt", "workflow")?;

    let origin = parse_origin(req_table(root, "origin", "origin")?)?;
    let destination = parse_destination(req_table(root, "destination", "destination")?)?;

    let paths_tbl = req_table(root, "paths", "paths")?;
    refuse_unknown_keys(paths_tbl, &["origin", "destination"], "paths")?;
    let origin_paths = parse_path_filter(
        req_table(paths_tbl, "origin", "paths.origin")?,
        "paths.origin",
    )?;
    let destination_paths = parse_path_filter(
        req_table(paths_tbl, "destination", "paths.destination")?,
        "paths.destination",
    )?;

    let authoring = parse_authoring(req_table(root, "authoring", "authoring")?)?;
    let transformations = match root.get("transformations") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| parse_transformation(item, i))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(ConfigError::new(
                "transformations",
                "must be an array of tables",
            ))
        }
    };

    let mut lint_keep_references = false;
    let mut lint_private_strip = false;
    let mut lint_ids = None;
    let mut lint_id_regexes = Vec::new();
    let (lint_private, lint_allow) = match root.get("lint") {
        None => (None, Vec::new()),
        Some(Value::Table(tbl)) => {
            refuse_unknown_keys(
                tbl,
                &["private", "allow", "references", "private_action", "ids"],
                "lint",
            )?;
            if let Some(ids) = opt_str_array(tbl, "ids", "lint")? {
                lint_id_regexes = compile_regexes(&ids, "lint.ids")?;
                lint_ids = Some(ids);
            }
            lint_private_strip = match tbl.get("private_action") {
                None => false,
                Some(Value::String(v)) if v == "refuse" => false,
                Some(Value::String(v)) if v == "strip" => true,
                Some(_) => {
                    return Err(ConfigError::new(
                        "lint.private_action",
                        "must be \"refuse\" or \"strip\"",
                    ))
                }
            };
            lint_keep_references = match tbl.get("references") {
                None => false,
                Some(Value::String(v)) if v == "refuse" => false,
                Some(Value::String(v)) if v == "keep" => true,
                Some(_) => {
                    return Err(ConfigError::new(
                        "lint.references",
                        "must be \"refuse\" or \"keep\"",
                    ))
                }
            };
            let allow = match tbl.get("allow") {
                None => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| parse_lint_allow(item, index))
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => {
                    return Err(ConfigError::new("lint.allow", "must be an array of tables"))
                }
            };
            (opt_str(tbl, "private", "lint")?, allow)
        }
        Some(_) => return Err(ConfigError::new("lint", "must be a table")),
    };
    let scrub_private = match root.get("scrub") {
        None => None,
        Some(Value::Table(tbl)) => {
            refuse_unknown_keys(tbl, &["private"], "scrub")?;
            opt_str(tbl, "private", "scrub")?
        }
        Some(_) => return Err(ConfigError::new("scrub", "must be a table")),
    };
    let cascade_rust = match root.get("cascade") {
        None => None,
        Some(Value::Table(tbl)) => {
            refuse_unknown_keys(tbl, &["rust"], "cascade")?;
            match tbl.get("rust") {
                None => None,
                Some(Value::String(v)) if v == "module" => {
                    Some(crate::rust_cascade::RustCascade::Module)
                }
                Some(Value::String(v)) if v == "unlink" => {
                    Some(crate::rust_cascade::RustCascade::Unlink)
                }
                Some(Value::String(v)) if v == "auto" => {
                    Some(crate::rust_cascade::RustCascade::Auto)
                }
                Some(_) => {
                    return Err(ConfigError::new(
                        "cascade.rust",
                        "must be \"module\", \"unlink\" or \"auto\"",
                    ))
                }
            }
        }
        Some(_) => return Err(ConfigError::new("cascade", "must be a table")),
    };
    let verify = match root.get("verify") {
        None => None,
        Some(Value::Table(tbl)) => Some(parse_verify(tbl)?),
        Some(_) => return Err(ConfigError::new("verify", "must be a table")),
    };
    let gate = match root.get("gate") {
        None => None,
        Some(Value::Table(tbl)) => Some(crate::gate::GateConfig::parse(tbl)?),
        Some(_) => return Err(ConfigError::new("gate", "must be a table")),
    };
    if origin.subjects_clean && lint_private.is_none() {
        return Err(ConfigError::new(
            "origin.subjects",
            "\"clean\" reads the private patterns: it needs [lint] private",
        ));
    }

    Ok(Workflow {
        name,
        mode,
        dry_run,
        receipt,
        origin,
        destination,
        origin_paths,
        destination_paths,
        authoring,
        transformations,
        lint_private,
        lint_allow,
        lint_keep_references,
        lint_ids,
        lint_id_regexes,
        lint_private_strip,
        scrub_private,
        gate,
        cascade_rust,
        verify,
    })
}

fn parse_verify(tbl: &toml::Table) -> Result<Verify, ConfigError> {
    refuse_unknown_keys(tbl, &["commands", "env", "dir", "on_fail"], "verify")?;
    let commands = match tbl.get("commands") {
        None => return Err(ConfigError::new("verify.commands", "missing")),
        Some(Value::Array(items)) if items.is_empty() => {
            return Err(ConfigError::new("verify.commands", "empty"))
        }
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                Value::Array(argv) if !argv.is_empty() => {
                    str_array(argv, &format!("verify.commands[{i}]"))
                }
                _ => Err(ConfigError::new(
                    format!("verify.commands[{i}]"),
                    "must be a non-empty array of strings",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(ConfigError::new(
                "verify.commands",
                "must be an array of arrays of strings",
            ))
        }
    };
    let env = match tbl.get("env") {
        None => Vec::new(),
        Some(Value::Table(env)) => env
            .iter()
            .map(|(key, value)| match value {
                Value::String(v) => Ok((key.clone(), v.clone())),
                _ => Err(ConfigError::new(
                    format!("verify.env.{key}"),
                    "must be a string",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(ConfigError::new("verify.env", "must be a table")),
    };
    let refuse = match tbl.get("on_fail") {
        None => true,
        Some(Value::String(v)) if v == "refuse" => true,
        Some(Value::String(v)) if v == "report" => false,
        Some(_) => {
            return Err(ConfigError::new(
                "verify.on_fail",
                "must be \"refuse\" or \"report\"",
            ))
        }
    };
    Ok(Verify {
        commands,
        env,
        dir: opt_str(tbl, "dir", "verify")?,
        refuse,
    })
}

fn parse_lint_allow(value: &Value, index: usize) -> Result<LintAllow, ConfigError> {
    let path = format!("lint.allow[{index}]");
    let tbl = value
        .as_table()
        .ok_or_else(|| ConfigError::new(&path, "must be a table"))?;
    refuse_unknown_keys(tbl, &["paths", "patterns"], &path)?;
    let paths = req_str_array(tbl, "paths", &path)?;
    let patterns = req_positive_usize_array(tbl, "patterns", &path)?;
    let matcher = globset(&paths, &format!("{path}.paths"))?;
    Ok(LintAllow {
        paths,
        patterns,
        matcher,
    })
}

fn parse_origin(tbl: &toml::Table) -> Result<Origin, ConfigError> {
    refuse_unknown_keys(
        tbl,
        &["path", "url", "ref", "since", "subjects", "auth"],
        "origin",
    )?;
    let location = parse_location(tbl, "origin")?;
    let ref_name = req_str(tbl, "ref", "origin")?;
    let since = opt_str(tbl, "since", "origin")?;
    let subjects_clean = match opt_str(tbl, "subjects", "origin")?.as_deref() {
        None => false,
        Some("clean") => true,
        Some(_) => {
            return Err(ConfigError::new(
                "origin.subjects",
                "the only value is \"clean\"",
            ))
        }
    };
    let auth = match tbl.get("auth") {
        None => None,
        Some(v) => Some(parse_auth(v, "origin.auth")?),
    };
    Ok(Origin {
        location,
        ref_name,
        since,
        subjects_clean,
        auth,
    })
}

fn parse_destination(tbl: &toml::Table) -> Result<Destination, ConfigError> {
    refuse_unknown_keys(
        tbl,
        &["path", "url", "fetch", "push", "auth"],
        "destination",
    )?;
    let location = parse_location(tbl, "destination")?;
    let fetch = req_str(tbl, "fetch", "destination")?;
    let push = req_str(tbl, "push", "destination")?;
    let auth = match tbl.get("auth") {
        None => None,
        Some(v) => Some(parse_auth(v, "destination.auth")?),
    };
    Ok(Destination {
        location,
        fetch,
        push,
        auth,
    })
}

fn parse_location(tbl: &toml::Table, path: &str) -> Result<Location, ConfigError> {
    let loc_path = opt_str(tbl, "path", path)?;
    let url = opt_str(tbl, "url", path)?;
    match (&loc_path, &url) {
        (Some(_), Some(_)) => Err(ConfigError::new(path, "set path or url, not both")),
        (None, None) => Err(ConfigError::new(path, "set path or url")),
        _ => Ok(Location {
            path: loc_path,
            url,
        }),
    }
}

fn parse_auth(value: &Value, path: &str) -> Result<Auth, ConfigError> {
    let tbl = value
        .as_table()
        .ok_or_else(|| ConfigError::new(path, "must be a table"))?;
    refuse_unknown_keys(tbl, &["source", "name", "url"], path)?;
    let source = req_str(tbl, "source", path)?;
    match source.as_str() {
        "env" => {
            if tbl.get("url").is_some() {
                return Err(ConfigError::new(
                    format!("{path}.url"),
                    "not valid when source is \"env\"",
                ));
            }
            Ok(Auth::Env {
                name: req_str(tbl, "name", path)?,
            })
        }
        "helper" => {
            if tbl.get("url").is_some() {
                return Err(ConfigError::new(
                    format!("{path}.url"),
                    "not valid when source is \"helper\"",
                ));
            }
            Ok(Auth::Helper {
                name: req_str(tbl, "name", path)?,
            })
        }
        "instead_of" => {
            if tbl.get("name").is_some() {
                return Err(ConfigError::new(
                    format!("{path}.name"),
                    "not valid when source is \"instead_of\"",
                ));
            }
            Ok(Auth::InsteadOf {
                url: req_str(tbl, "url", path)?,
            })
        }
        _ => Err(ConfigError::new(
            format!("{path}.source"),
            "must be \"env\", \"helper\", or \"instead_of\"",
        )),
    }
}

fn parse_path_filter(tbl: &toml::Table, path: &str) -> Result<PathFilter, ConfigError> {
    refuse_unknown_keys(tbl, &["include", "exclude"], path)?;
    let include = req_str_array(tbl, "include", path)?;
    let exclude = opt_str_array(tbl, "exclude", path)?.unwrap_or_default();
    let include_set = globset(&include, &format!("{path}.include"))?;
    let exclude_set = globset(&exclude, &format!("{path}.exclude"))?;
    Ok(PathFilter {
        include,
        exclude,
        include_set,
        exclude_set,
    })
}

fn parse_authoring(tbl: &toml::Table) -> Result<Authoring, ConfigError> {
    refuse_unknown_keys(tbl, &["default", "map"], "authoring")?;
    let default = parse_identity(req_value(tbl, "default", "authoring")?, "authoring.default")?;
    let map = match tbl.get("map") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| parse_author_map(item, i))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(ConfigError::new(
                "authoring.map",
                "must be an array of tables",
            ))
        }
    };
    Ok(Authoring { default, map })
}

fn parse_author_map(value: &Value, index: usize) -> Result<AuthorMap, ConfigError> {
    let path = format!("authoring.map[{index}]");
    let tbl = value
        .as_table()
        .ok_or_else(|| ConfigError::new(&path, "must be a table"))?;
    refuse_unknown_keys(tbl, &["from", "to"], &path)?;
    Ok(AuthorMap {
        from: parse_identity(req_value(tbl, "from", &path)?, &format!("{path}.from"))?,
        to: parse_identity(req_value(tbl, "to", &path)?, &format!("{path}.to"))?,
    })
}

fn parse_identity(value: &Value, path: &str) -> Result<Identity, ConfigError> {
    let tbl = value
        .as_table()
        .ok_or_else(|| ConfigError::new(path, "must be a table"))?;
    refuse_unknown_keys(tbl, &["name", "email"], path)?;
    Ok(Identity {
        name: req_str(tbl, "name", path)?,
        email: req_str(tbl, "email", path)?,
    })
}

fn parse_transformation(value: &Value, index: usize) -> Result<Transformation, ConfigError> {
    let path = format!("transformations[{index}]");
    let tbl = value
        .as_table()
        .ok_or_else(|| ConfigError::new(&path, "must be a table"))?;
    let kind = req_str(tbl, "type", &path)?;
    match kind.as_str() {
        "strip_paths" => {
            refuse_unknown_keys(tbl, &["type", "globs"], &path)?;
            let globs = req_str_array(tbl, "globs", &path)?;
            let matcher = globset(&globs, &format!("{path}.globs"))?;
            Ok(Transformation::StripPaths { globs, matcher })
        }
        "strip_references" => {
            refuse_unknown_keys(tbl, &["type", "patterns"], &path)?;
            let patterns = req_str_array(tbl, "patterns", &path)?;
            let regexes = compile_regexes(&patterns, &format!("{path}.patterns"))?;
            Ok(Transformation::StripReferences { patterns, regexes })
        }
        "rewrite_messages" => {
            refuse_unknown_keys(
                tbl,
                &["type", "hook", "prompt_budget_tokens", "fallback"],
                &path,
            )?;
            let hook = opt_str(tbl, "hook", &path)?;
            if let Some(name) = hook.as_deref() {
                if name != "low_cost_model" {
                    return Err(ConfigError::new(
                        format!("{path}.hook"),
                        "must be \"low_cost_model\"",
                    ));
                }
            }
            let prompt_budget_tokens = opt_u32(tbl, "prompt_budget_tokens", &path)?.unwrap_or(512);
            let fallback = opt_str(tbl, "fallback", &path)?
                .unwrap_or_else(|| "first_line_strip_references".to_string());
            if fallback != "first_line_strip_references" {
                return Err(ConfigError::new(
                    format!("{path}.fallback"),
                    "must be \"first_line_strip_references\"",
                ));
            }
            Ok(Transformation::RewriteMessages {
                hook,
                prompt_budget_tokens,
                fallback,
            })
        }
        "replace_text" => {
            refuse_unknown_keys(
                tbl,
                &[
                    "type",
                    "pattern",
                    "replacement",
                    "on",
                    "paths",
                    "exclude",
                    "ignore_lines",
                ],
                &path,
            )?;
            let pattern = req_str(tbl, "pattern", &path)?;
            let replacement = req_str_allow_empty(tbl, "replacement", &path)?;
            let on = parse_on(tbl, &path)?;
            let regex = Regex::new(&pattern).map_err(|err| {
                ConfigError::new(format!("{path}.pattern"), format!("invalid regex: {err}"))
            })?;
            let scope = parse_scope(tbl, &path)?;
            Ok(Transformation::ReplaceText {
                pattern,
                replacement,
                on,
                regex,
                scope,
            })
        }
        "replace_pairs" => {
            refuse_unknown_keys(
                tbl,
                &[
                    "type",
                    "pairs",
                    "on",
                    "paths",
                    "exclude",
                    "ignore_lines",
                    "follow_moves",
                ],
                &path,
            )?;
            let pairs = req_pairs(tbl, "pairs", &path)?;
            let on = parse_on(tbl, &path)?;
            let scope = parse_scope(tbl, &path)?;
            let follow_moves = opt_bool(tbl, "follow_moves", &path)?.unwrap_or(false);
            Ok(Transformation::ReplacePairs {
                pairs,
                on,
                scope,
                follow_moves,
            })
        }
        "strip_matching" => {
            refuse_unknown_keys(
                tbl,
                &["type", "paths", "exclude", "patterns", "private"],
                &path,
            )?;
            req_str_array(tbl, "paths", &path)?;
            let private = opt_bool(tbl, "private", &path)?.unwrap_or(false);
            let patterns = if private {
                opt_str_array(tbl, "patterns", &path)?.unwrap_or_default()
            } else {
                req_str_array(tbl, "patterns", &path)?
            };
            let regexes = compile_regexes(&patterns, &format!("{path}.patterns"))?;
            let scope = parse_scope(tbl, &path)?;
            Ok(Transformation::StripMatching {
                patterns,
                regexes,
                scope,
                private,
                private_regexes: Vec::new(),
            })
        }
        "move_paths" => {
            refuse_unknown_keys(tbl, &["type", "pairs", "paths", "exclude"], &path)?;
            let pairs = req_pairs(tbl, "pairs", &path)?;
            let scope = parse_scope(tbl, &path)?;
            Ok(Transformation::MovePaths { pairs, scope })
        }
        "rename_paths" => {
            refuse_unknown_keys(tbl, &["type", "from", "to"], &path)?;
            Ok(Transformation::RenamePaths {
                from: req_str(tbl, "from", &path)?,
                to: req_str(tbl, "to", &path)?,
            })
        }
        "strip_comments" => {
            refuse_unknown_keys(
                tbl,
                &["type", "paths", "exclude", "languages", "keep_docs"],
                &path,
            )?;
            let languages = match opt_str_array(tbl, "languages", &path)? {
                None => vec![
                    crate::comments::Lang::Rust,
                    crate::comments::Lang::Script,
                    crate::comments::Lang::Jsx,
                    crate::comments::Lang::Python,
                    crate::comments::Lang::Swift,
                ],
                Some(names) => names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        crate::comments::Lang::from_name(name).ok_or_else(|| {
                            ConfigError::new(
                                format!("{path}.languages[{i}]"),
                                "must be rust, typescript, tsx, python or swift",
                            )
                        })
                    })
                    .collect::<Result<_, _>>()?,
            };
            let keep_docs = opt_bool(tbl, "keep_docs", &path)?.unwrap_or(true);
            let scope = parse_scope(tbl, &path)?;
            Ok(Transformation::StripComments {
                languages,
                keep_docs,
                scope,
            })
        }
        "drop_empty_commits" => {
            refuse_unknown_keys(tbl, &["type"], &path)?;
            Ok(Transformation::DropEmptyCommits)
        }
        _ => Err(ConfigError::new(
            format!("{path}.type"),
            format!("unknown type \"{kind}\""),
        )),
    }
}

fn parse_on(tbl: &toml::Table, path: &str) -> Result<ReplaceOn, ConfigError> {
    match req_str(tbl, "on", path)?.as_str() {
        "files" => Ok(ReplaceOn::Files),
        "messages" => Ok(ReplaceOn::Messages),
        "both" => Ok(ReplaceOn::Both),
        _ => Err(ConfigError::new(
            format!("{path}.on"),
            "must be \"files\", \"messages\", or \"both\"",
        )),
    }
}

/// A non-empty array of `[from, to]` string pairs; `from` is never empty.
fn req_pairs(
    tbl: &toml::Table,
    key: &str,
    path: &str,
) -> Result<Vec<(String, String)>, ConfigError> {
    let key_path = format!("{path}.{key}");
    let items = req_value(tbl, key, path)?
        .as_array()
        .ok_or_else(|| ConfigError::new(&key_path, "must be an array of [from, to] pairs"))?;
    if items.is_empty() {
        return Err(ConfigError::new(&key_path, "must name at least one pair"));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let at = format!("{key_path}[{i}]");
            match item.as_array().map(Vec::as_slice) {
                Some([Value::String(from), Value::String(to)]) if !from.is_empty() => {
                    Ok((from.clone(), to.clone()))
                }
                Some([Value::String(_), Value::String(_)]) => {
                    Err(ConfigError::new(at, "from must not be empty"))
                }
                _ => Err(ConfigError::new(at, "must be a [from, to] pair of strings")),
            }
        })
        .collect()
}

/// The optional `paths`, `exclude` and `ignore_lines` keys of a text transformation.
fn parse_scope(tbl: &toml::Table, path: &str) -> Result<Scope, ConfigError> {
    let paths = opt_str_array(tbl, "paths", path)?;
    let exclude = opt_str_array(tbl, "exclude", path)?.unwrap_or_default();
    let ignore_lines = opt_str_array(tbl, "ignore_lines", path)?.unwrap_or_default();
    let paths_set = paths
        .as_ref()
        .map(|globs| globset(globs, &format!("{path}.paths")))
        .transpose()?;
    let exclude_set = if exclude.is_empty() {
        None
    } else {
        Some(globset(&exclude, &format!("{path}.exclude"))?)
    };
    let ignore_regexes = compile_regexes(&ignore_lines, &format!("{path}.ignore_lines"))?;
    Ok(Scope {
        paths: paths.unwrap_or_default(),
        exclude,
        ignore_lines,
        paths_set,
        exclude_set,
        ignore_regexes,
    })
}

fn compile_regexes(patterns: &[String], key: &str) -> Result<Vec<Regex>, ConfigError> {
    patterns
        .iter()
        .enumerate()
        .map(|(i, pattern)| {
            Regex::new(pattern).map_err(|err| {
                ConfigError::new(format!("{key}[{i}]"), format!("invalid regex: {err}"))
            })
        })
        .collect()
}

fn refuse_unknown_keys(tbl: &toml::Table, allowed: &[&str], path: &str) -> Result<(), ConfigError> {
    for key in tbl.keys() {
        if !allowed.contains(&key.as_str()) {
            let named = if path == "document" {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            return Err(ConfigError::new(named, "unknown key"));
        }
    }
    Ok(())
}

fn req_table<'a>(
    tbl: &'a toml::Table,
    key: &str,
    named: &str,
) -> Result<&'a toml::Table, ConfigError> {
    match tbl.get(key) {
        None => Err(ConfigError::new(named, "missing")),
        Some(Value::Table(inner)) => Ok(inner),
        Some(_) => Err(ConfigError::new(named, "must be a table")),
    }
}

fn req_value<'a>(tbl: &'a toml::Table, key: &str, path: &str) -> Result<&'a Value, ConfigError> {
    tbl.get(key)
        .ok_or_else(|| ConfigError::new(format!("{path}.{key}"), "missing"))
}

fn req_str(tbl: &toml::Table, key: &str, path: &str) -> Result<String, ConfigError> {
    match tbl.get(key) {
        None => Err(ConfigError::new(format!("{path}.{key}"), "missing")),
        Some(Value::String(s)) if s.is_empty() => {
            Err(ConfigError::new(format!("{path}.{key}"), "empty"))
        }
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be a string",
        )),
    }
}

fn req_str_allow_empty(tbl: &toml::Table, key: &str, path: &str) -> Result<String, ConfigError> {
    match tbl.get(key) {
        None => Err(ConfigError::new(format!("{path}.{key}"), "missing")),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be a string",
        )),
    }
}

fn opt_str(tbl: &toml::Table, key: &str, path: &str) -> Result<Option<String>, ConfigError> {
    match tbl.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if s.is_empty() => {
            Err(ConfigError::new(format!("{path}.{key}"), "empty"))
        }
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be a string",
        )),
    }
}

fn opt_bool(tbl: &toml::Table, key: &str, path: &str) -> Result<Option<bool>, ConfigError> {
    match tbl.get(key) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(_) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be a boolean",
        )),
    }
}

fn opt_u32(tbl: &toml::Table, key: &str, path: &str) -> Result<Option<u32>, ConfigError> {
    match tbl.get(key) {
        None => Ok(None),
        Some(Value::Integer(n)) if *n >= 0 && *n <= u32::MAX as i64 => Ok(Some(*n as u32)),
        Some(Value::Integer(_)) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be a non-negative integer that fits in 32 bits",
        )),
        Some(_) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be an integer",
        )),
    }
}

fn req_str_array(tbl: &toml::Table, key: &str, path: &str) -> Result<Vec<String>, ConfigError> {
    match tbl.get(key) {
        None => Err(ConfigError::new(format!("{path}.{key}"), "missing")),
        Some(Value::Array(items)) => str_array(items, &format!("{path}.{key}")),
        Some(_) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be an array of strings",
        )),
    }
}

fn req_positive_usize_array(
    tbl: &toml::Table,
    key: &str,
    path: &str,
) -> Result<Vec<usize>, ConfigError> {
    let items = match tbl.get(key) {
        None => return Err(ConfigError::new(format!("{path}.{key}"), "missing")),
        Some(Value::Array(items)) if items.is_empty() => {
            return Err(ConfigError::new(format!("{path}.{key}"), "empty"))
        }
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(ConfigError::new(
                format!("{path}.{key}"),
                "must be an array of positive integers",
            ))
        }
    };
    items
        .iter()
        .enumerate()
        .map(|(index, value)| match value {
            Value::Integer(number) if *number > 0 && (*number as u128) <= usize::MAX as u128 => {
                Ok(*number as usize)
            }
            Value::Integer(_) => Err(ConfigError::new(
                format!("{path}.{key}[{index}]"),
                "must be a positive integer",
            )),
            _ => Err(ConfigError::new(
                format!("{path}.{key}[{index}]"),
                "must be a positive integer",
            )),
        })
        .collect()
}

fn opt_str_array(
    tbl: &toml::Table,
    key: &str,
    path: &str,
) -> Result<Option<Vec<String>>, ConfigError> {
    match tbl.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => Ok(Some(str_array(items, &format!("{path}.{key}"))?)),
        Some(_) => Err(ConfigError::new(
            format!("{path}.{key}"),
            "must be an array of strings",
        )),
    }
}

fn str_array(items: &[Value], path: &str) -> Result<Vec<String>, ConfigError> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| match item {
            Value::String(s) if s.is_empty() => {
                Err(ConfigError::new(format!("{path}[{i}]"), "empty"))
            }
            Value::String(s) => Ok(s.clone()),
            _ => Err(ConfigError::new(format!("{path}[{i}]"), "must be a string")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[workflow]
name = "example"
mode = "iterative"
dry_run = false
receipt = "receipts/example.toml"

[origin]
path = "../private-repo"
ref = "main"
auth = { source = "env", name = "COPYBERA_ORIGIN_AUTH" }

[destination]
url = "git@github.com:example/public.git"
fetch = "main"
push = "main"
auth = { source = "helper", name = "gh" }

[paths.origin]
include = ["**"]
exclude = ["**/*.secret"]

[paths.destination]
include = ["**"]
exclude = []

[authoring]
default = { name = "Project Authors", email = "authors@example.invalid" }

[[authoring.map]]
from = { name = "Ada", email = "ada@internal.example" }
to = { name = "Ada", email = "ada@example.invalid" }

[[transformations]]
type = "strip_paths"
globs = ["tmp/**"]
"#;

    const MONOREPO: &str = r#"
[workflow]
name = "monorepo-public"
mode = "squash"
dry_run = false
receipt = "receipts/monorepo-public.toml"

[origin]
path = "../monorepo"
ref = "v1.0.0"
auth = { source = "env", name = "COPYBERA_ORIGIN_AUTH" }

[destination]
url = "git@github.com:example/monorepo.git"
fetch = "main"
push = "main"
auth = { source = "instead_of", url = "ssh://git@github.com/" }

[paths.origin]
include = [
  "crates/protocol/**",
  "crates/server/**",
  "docs/api/**",
  "apps/**",
  "tools/release/**",
]
exclude = [
  ".tracker/**",
  "docs/internal/**",
  "docs/reviews/**",
  "tools/ci/**",
  ".cache/**",
]

[paths.destination]
include = [
  "crates/protocol/**",
  "crates/server/**",
  "docs/api/**",
  "apps/**",
  "tools/release/**",
]
exclude = []

[authoring]
default = { name = "Project Authors", email = "authors@example.invalid" }

[[transformations]]
type = "strip_references"
patterns = ["TICKET-[0-9]+", "\\.tracker/"]

[[transformations]]
type = "rewrite_messages"
hook = "low_cost_model"
prompt_budget_tokens = 512
fallback = "first_line_strip_references"

[[transformations]]
type = "drop_empty_commits"
"#;

    #[test]
    fn parses_the_design_example() {
        let wf = parse_workflow(EXAMPLE).unwrap();
        assert_eq!(wf.name, "example");
        assert_eq!(wf.mode, Mode::Iterative);
        assert_eq!(wf.origin.ref_name, "main");
        assert!(wf.origin.location.path.is_some());
        assert!(wf.destination.location.url.is_some());
        assert_eq!(wf.transformations.len(), 1);
        assert_eq!(wf.authoring.map.len(), 1);
    }

    #[test]
    fn parses_a_monorepo_ruleset() {
        let wf = parse_workflow(MONOREPO).unwrap();
        assert_eq!(wf.name, "monorepo-public");
        assert_eq!(wf.mode, Mode::Squash);
        assert_eq!(wf.transformations.len(), 3);
        assert!(matches!(wf.destination.auth, Some(Auth::InsteadOf { .. })));
        // With no `[lint] ids`, the lint refuses what strip_references strips.
        assert_eq!(wf.lint_references().0, ["TICKET-[0-9]+", "\\.tracker/"]);
    }

    #[test]
    fn lint_ids_replace_the_strip_references_patterns() {
        let with_ids = MONOREPO.replace(
            "[authoring]",
            "[lint]\nids = [\"JIRA-[0-9]+\"]\n\n[authoring]",
        );
        let wf = parse_workflow(&with_ids).unwrap();
        let (ids, regexes) = wf.lint_references();
        assert_eq!(ids, ["JIRA-[0-9]+"]);
        assert!(regexes[0].is_match("see JIRA-12"));
        // An empty list turns the reference check off.
        let none = MONOREPO.replace("[authoring]", "[lint]\nids = []\n\n[authoring]");
        assert!(parse_workflow(&none)
            .unwrap()
            .lint_references()
            .0
            .is_empty());
        let bad = MONOREPO.replace("[authoring]", "[lint]\nids = [\"(\"]\n\n[authoring]");
        assert!(parse_workflow(&bad)
            .unwrap_err()
            .to_string()
            .contains("lint.ids"));
    }

    #[test]
    fn refuses_both_origin_path_and_url() {
        let err = parse_workflow(
            r#"
[workflow]
name = "x"
mode = "iterative"
[origin]
path = "a"
url = "b"
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
default = { name = "A", email = "a@b.c" }
"#,
        )
        .unwrap_err();
        assert_eq!(err.key, "origin");
        assert!(err.message.contains("not both"), "{err}");
    }

    #[test]
    fn refuses_unknown_mode_naming_the_key() {
        let err = parse_workflow(
            r#"
[workflow]
name = "x"
mode = "change_request"
[origin]
path = "a"
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
default = { name = "A", email = "a@b.c" }
"#,
        )
        .unwrap_err();
        assert_eq!(err.key, "workflow.mode");
    }

    #[test]
    fn refuses_missing_paths_origin_naming_the_full_key() {
        let err = parse_workflow(
            r#"
[workflow]
name = "x"
mode = "iterative"
[origin]
path = "a"
ref = "main"
[destination]
path = "d"
fetch = "main"
push = "main"
[paths]
[paths.destination]
include = ["**"]
[authoring]
default = { name = "A", email = "a@b.c" }
"#,
        )
        .unwrap_err();
        assert_eq!(err.key, "paths.origin");
        assert_eq!(err.message, "missing");
        assert_eq!(err.to_string(), "paths.origin: missing");
    }

    #[test]
    fn refuses_missing_name() {
        let err = parse_workflow(
            r#"
[workflow]
mode = "iterative"
[origin]
path = "a"
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
default = { name = "A", email = "a@b.c" }
"#,
        )
        .unwrap_err();
        assert_eq!(err.key, "workflow.name");
        assert_eq!(err.message, "missing");
        assert_eq!(err.to_string(), "workflow.name: missing");
    }

    #[test]
    fn refuses_unknown_transformation_type() {
        let err = parse_workflow(
            r#"
[workflow]
name = "x"
mode = "iterative"
[origin]
path = "a"
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
default = { name = "A", email = "a@b.c" }
[[transformations]]
type = "starlark"
"#,
        )
        .unwrap_err();
        assert_eq!(err.key, "transformations[0].type");
        assert!(err.message.contains("starlark"), "{err}");
    }

    #[test]
    fn refuses_unknown_top_level_key() {
        let err = parse_workflow(
            r#"
[workflow]
name = "x"
mode = "iterative"
[origin]
path = "a"
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
default = { name = "A", email = "a@b.c" }
[secrets]
token = "no"
"#,
        )
        .unwrap_err();
        assert_eq!(err.key, "secrets");
        assert_eq!(err.message, "unknown key");
    }

    #[test]
    fn refuses_invalid_reference_regex() {
        let err = parse_workflow(
            r#"
[workflow]
name = "x"
mode = "iterative"
[origin]
path = "a"
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
default = { name = "A", email = "a@b.c" }
[[transformations]]
type = "strip_references"
patterns = ["("]
"#,
        )
        .unwrap_err();
        assert_eq!(err.key, "transformations[0].patterns[0]");
        assert!(err.message.contains("invalid regex"), "{err}");
    }

    // A workflow names the private-patterns file its lint reads (the file lives outside git).
    #[test]
    fn parses_cascade_and_verify_and_names_the_refusing_key() {
        let wf = parse_workflow(&format!(
            "{EXAMPLE}\n[cascade]\nrust = \"module\"\n\n[verify]\ncommands = [[\"cargo\", \"metadata\"], [\"true\"]]\n\
             env = {{ A = \"b\" }}\ndir = \"tree\"\non_fail = \"report\"\n"
        ))
        .unwrap();
        assert_eq!(
            wf.cascade_rust,
            Some(crate::rust_cascade::RustCascade::Module)
        );
        let verify = wf.verify.unwrap();
        assert_eq!(verify.commands.len(), 2);
        assert_eq!(verify.env, vec![("A".to_string(), "b".to_string())]);
        assert_eq!(verify.dir.as_deref(), Some("tree"));
        assert!(!verify.refuse);
        let wf = parse_workflow(EXAMPLE).unwrap();
        assert!(wf.cascade_rust.is_none() && wf.verify.is_none());
        for (extra, key) in [
            ("[cascade]\nrust = \"all\"\n", "cascade.rust"),
            ("[cascade]\npython = \"module\"\n", "cascade"),
            ("[verify]\ncommands = []\n", "verify.commands"),
            ("[verify]\ncommands = [[]]\n", "verify.commands[0]"),
            (
                "[verify]\ncommands = [[\"x\"]]\non_fail = \"ignore\"\n",
                "verify.on_fail",
            ),
            (
                "[verify]\ncommands = [[\"x\"]]\nenv = { A = 1 }\n",
                "verify.env.A",
            ),
        ] {
            let err = parse_workflow(&format!("{EXAMPLE}\n{extra}")).unwrap_err();
            assert!(err.to_string().contains(key), "{extra}: {err}");
        }
    }

    #[test]
    fn parses_the_lint_private_patterns_file_and_refuses_unknown_lint_keys() {
        let wf =
            parse_workflow(&format!("{EXAMPLE}\n[lint]\nprivate = \"private.txt\"\n")).unwrap();
        assert_eq!(wf.lint_private.as_deref(), Some("private.txt"));
        assert_eq!(parse_workflow(EXAMPLE).unwrap().lint_private, None);

        let err = parse_workflow(&format!("{EXAMPLE}\n[lint]\nprivate = \"\"\n")).unwrap_err();
        assert_eq!(err.key, "lint.private");
        let err = parse_workflow(&format!("{EXAMPLE}\n[lint]\nwords = [\"x\"]\n")).unwrap_err();
        assert_eq!(err.key, "lint.words");
        assert_eq!(err.message, "unknown key");
    }

    // A public export can start at a recent commit whose range's subjects lint clean.
    #[test]
    fn origin_subjects_clean_needs_the_private_patterns() {
        let with = |origin_extra: &str, lint: &str| {
            parse_workflow(&EXAMPLE.replacen(
                "[origin]\n",
                &format!("{lint}[origin]\n{origin_extra}"),
                1,
            ))
        };
        let wf = with(
            "subjects = \"clean\"\n",
            "[lint]\nprivate = \"private.txt\"\n\n",
        )
        .unwrap();
        assert!(wf.origin.subjects_clean);
        assert!(!parse_workflow(EXAMPLE).unwrap().origin.subjects_clean);
        let err = with("subjects = \"clean\"\n", "").unwrap_err();
        assert_eq!(err.key, "origin.subjects");
        let err = with(
            "subjects = \"tidy\"\n",
            "[lint]\nprivate = \"private.txt\"\n\n",
        )
        .unwrap_err();
        assert_eq!(err.key, "origin.subjects");
    }
}
