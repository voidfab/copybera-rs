//! The exported-tree gate. A tree is scanned by class and the report holds classes, counts and file paths only, never
//! the text a class matched.
//!
//! Every file is read as bytes (a Mach-O code signature blob masked first: it carries the signer's own public
//! identity), the path is read too for the classes a path can carry, and `[gate]` supplies what only the machine
//! running the export knows (networks, placeholders, legacy
//! names, private paths) plus `[gate.licence]` (the redistribution list and the carried-upstreams manifest).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use globset::GlobSet;
use regex::bytes::Regex as BytesRegex;
use regex::Regex;

use crate::error::ConfigError;
use crate::globmatch::globset;
use crate::transform::Blob;

/// Every class, in report order. `lan-example` is counted and never a finding.
pub const CLASSES: &[&str] = &[
    "home-path",
    "volume-path",
    "email",
    "lan-address",
    "lan-example",
    "lan-host",
    "token",
    "secret-ref",
    "source-map",
    "person-entity",
    "private-term",
    "private-path",
    "audio",
    "leftover",
    "leftover-path",
    "licence",
    "notice",
];

// Byte patterns with ASCII word semantics (`(?-u)`): a binary file is matched as bytes, not as UTF-8 text.
static HOME_USERS: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"/Users/([A-Za-z][A-Za-z0-9._-]*)").unwrap());
static HOME_HOME: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"/home/([a-z_][a-z0-9_.-]*)/").unwrap());
static VOLUME: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"/Volumes/([A-Za-z0-9][A-Za-z0-9 ._-]*)").unwrap());
static EMAIL: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(
        r"(?-u)\b([A-Za-z0-9._%+-]+)@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,})\b",
    )
    .unwrap()
});
static ICON_DOMAIN: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"^[0-9]+x\.").unwrap());
static SECRET_REF: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"op://[A-Za-z0-9 ._/-]+").unwrap());
static SOURCE_MAP: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"sourceMappingURL=").unwrap());
// secret_shape_check.py's token families; a prefix must not follow a word character (checked after the match).
static TOKEN: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(concat!(
        r"(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,}|sk-(?:ant-|proj-)?[A-Za-z0-9_-]{32,}",
        r"|hf_[A-Za-z0-9]{30,}|xox[abprs]-[A-Za-z0-9-]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}",
        r"|eyJ[A-Za-z0-9_-]{15,}\.eyJ[A-Za-z0-9_-]{15,}\.[A-Za-z0-9_-]{15,})",
        r"|-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED )?PRIVATE KEY-----",
    ))
    .unwrap()
});
static TOKEN_PLACEHOLDER: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r"(?i)x{6,}|example|placeholder|redacted|dummy|fake|0{12,}").unwrap()
});
// LAN address candidates; the neighbours and the address are judged after the match.
static LAN: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r"(?:10|172|192|100)\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}").unwrap()
});
static LAN_HOST_LABEL: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"[A-Za-z0-9][A-Za-z0-9.-]*$").unwrap());
static ENTITY: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r"(?-u)\b(?:person|device_tracker)\.([a-z0-9_]{2,})\b").unwrap()
});
// Upstream author credits: an email in one of these files is the upstream's, published by it.
static CREDIT_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(LICEN[CS]E|COPYING|NOTICE|AUTHORS|CONTRIBUTORS|THIRD_PARTY_NOTICES)([-_.].*)?$",
    )
    .unwrap()
});
static LICENCE_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(LICEN[CS]E|COPYING)([-_.].*)?$").unwrap());
static NOTICE_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(NOTICE|LICEN[CS]E|COPYING|ATTRIBUTION)([-_.].*)?$").unwrap()
});
static SPDX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"SPDX-License-Identifier:\s*([A-Za-z0-9.+\-() ]+?)\s*(?:\*/|-->|$)").unwrap()
});
static OR_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+or\s+|\s*/\s*").unwrap());
static AND_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+and\s+|\s*\+\s*|;").unwrap());

/// An address names nobody when its domain ends in one of these (a file name: `icon@2x.png`).
const FILE_TLDS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "icns", "pdf", "patch", "diff", "json",
    "js", "ts", "css", "html", "md", "txt", "wav", "mp3", "mp4", "mov", "zip", "gz", "tar",
];
const EMAIL_DOMAINS: &[&str] = &["example.com", "example.org", "example.net"];
const EMAIL_TLDS: &[&str] = &[".example", ".invalid", ".test", ".localhost"];
const WORD: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_";

/// The licences that allow redistribution: the default of `[gate.licence] allowed`.
pub const DEFAULT_ALLOWED_LICENCES: &[&str] = &[
    "mit",
    "apache-2.0",
    "bsd-2-clause",
    "bsd-3-clause",
    "isc",
    "0bsd",
    "zlib",
    "unlicense",
    "cc0-1.0",
    "mpl-2.0",
    "ofl-1.1",
    "cc-by-4.0",
    "bsl-1.0",
    "unicode-3.0",
    "unicode-dfs-2016",
    "python-2.0",
    "x11",
];
/// A carried upstream's kinds in the carried-upstreams manifest: code or data it ships.
pub const DEFAULT_CARRIED_KINDS: &[&str] =
    &["port", "vendored", "patch", "data", "model", "weights"];
/// Source suffixes: a licence-named file with one is code, not a licence text.
pub const SOURCE_SUFFIXES: &[&str] = &[
    ".py", ".rs", ".ts", ".tsx", ".js", ".mjs", ".cjs", ".swift", ".sh", ".bash", ".zsh", ".plist",
    ".tmpl", ".toml", ".nix", ".pkl", ".yaml", ".yml", ".example", ".service", ".timer", ".env",
    ".h", ".c", ".m", ".kt", ".svelte", ".vue", ".html", ".css", ".just",
];
/// (marker in a licence file, its id); the marker that comes first in the text names it.
const LICENCE_TEXT: &[(&str, &str)] = &[
    ("gnu affero general public", "agpl-3.0"),
    ("gnu lesser general public", "lgpl"),
    ("gnu library general public", "lgpl"),
    ("gnu general public license", "gpl"),
    ("business source license", "busl-1.1"),
    ("server side public license", "sspl-1.0"),
    ("commons clause", "commons-clause"),
    ("functional source license", "fsl-1.1"),
    ("polyform", "polyform"),
    ("noncommercial", "non-commercial"),
    ("non-commercial", "non-commercial"),
    ("apache license", "apache-2.0"),
    ("mozilla public license", "mpl-2.0"),
    ("permission is hereby granted, free of charge", "mit"),
    ("isc license", "isc"),
    ("permission to use, copy, modify, and/or distribute", "isc"),
    (
        "redistribution and use in source and binary forms",
        "bsd-3-clause",
    ),
    ("this is free and unencumbered software", "unlicense"),
    ("sil open font license", "ofl-1.1"),
    ("creative commons attribution 4.0", "cc-by-4.0"),
    ("cc0 1.0", "cc0-1.0"),
    ("boost software license", "bsl-1.0"),
    ("zlib license", "zlib"),
    ("mit license", "mit"),
];

/// `[[gate.allow]]`: one class exempted under path globs, with the reason it is public there.
#[derive(Debug, Clone)]
pub struct GateAllow {
    pub class: String,
    pub paths: Vec<String>,
    pub reason: String,
    matcher: GlobSet,
}

/// `[gate.licence]`: what may be redistributed and where the carried upstreams are declared.
#[derive(Debug, Clone)]
pub struct LicenceConfig {
    /// Lower-case SPDX ids that may ship (default: [`DEFAULT_ALLOWED_LICENCES`]).
    pub allowed: Vec<String>,
    /// A TOML file inside the export with `[[source]]` tables (`kind`, `licence`, `paths`): the carried upstreams.
    pub carried: Option<String>,
    pub carried_kinds: Vec<String>,
    /// Files the export must carry (default: `LICENSE`, `THIRD_PARTY_NOTICES.md`).
    pub notices: Vec<String>,
    /// Suffixes whose first 40 lines are read for an SPDX header (default: [`SOURCE_SUFFIXES`] and `.md`).
    pub spdx_suffixes: Vec<String>,
    /// With the origin's paths: a licence or notice file the export dropped while files beside it were exported.
    pub dropped_notices: bool,
}

impl Default for LicenceConfig {
    fn default() -> Self {
        let mut spdx_suffixes: Vec<String> =
            SOURCE_SUFFIXES.iter().map(|s| s.to_string()).collect();
        spdx_suffixes.push(".md".into());
        Self {
            allowed: DEFAULT_ALLOWED_LICENCES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            carried: None,
            carried_kinds: DEFAULT_CARRIED_KINDS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            notices: vec!["LICENSE".into(), "THIRD_PARTY_NOTICES.md".into()],
            spdx_suffixes,
            dropped_notices: true,
        }
    }
}

const LICENCE_KEYS: &[&str] = &[
    "allowed",
    "carried",
    "carried_kinds",
    "notices",
    "spdx_suffixes",
    "dropped_notices",
];

impl LicenceConfig {
    fn parse(tbl: &toml::Table) -> Result<Self, ConfigError> {
        for key in tbl.keys() {
            if !LICENCE_KEYS.contains(&key.as_str()) {
                return Err(ConfigError::new(
                    format!("gate.licence.{key}"),
                    "unknown key",
                ));
            }
        }
        let mut config = Self::default();
        let at = "gate.licence";
        if let Some(allowed) = strings_of(tbl, at, "allowed")? {
            config.allowed = allowed.iter().map(|s| s.to_lowercase()).collect();
        }
        if let Some(kinds) = strings_of(tbl, at, "carried_kinds")? {
            config.carried_kinds = kinds;
        }
        if let Some(notices) = strings_of(tbl, at, "notices")? {
            config.notices = notices;
        }
        if let Some(suffixes) = strings_of(tbl, at, "spdx_suffixes")? {
            config.spdx_suffixes = suffixes;
        }
        config.carried = match tbl.get("carried") {
            None => None,
            Some(toml::Value::String(path)) => Some(path.clone()),
            Some(_) => return Err(ConfigError::new("gate.licence.carried", "must be a string")),
        };
        config.dropped_notices = match tbl.get("dropped_notices") {
            None => true,
            Some(toml::Value::Boolean(b)) => *b,
            Some(_) => {
                return Err(ConfigError::new(
                    "gate.licence.dropped_notices",
                    "must be a boolean",
                ))
            }
        };
        Ok(config)
    }

    /// An SPDX-ish expression: OR / `/` picks any allowed alternative, AND / `+` needs all; a Commons Clause never.
    pub fn licence_ok(&self, expr: &str) -> bool {
        let text = expr.trim().to_lowercase().replace(['(', ')'], " ");
        if text.contains("commons clause") {
            return false;
        }
        OR_SPLIT.split(&text).any(|alt| {
            let ids: Vec<&str> = AND_SPLIT
                .split(alt)
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.split_whitespace().next())
                .collect();
            !ids.is_empty() && ids.iter().all(|id| self.allowed.iter().any(|a| a == id))
        })
    }
}

/// `[gate]`: what the gate judges against.
#[derive(Debug, Clone, Default)]
pub struct GateConfig {
    /// IPv4 networks (`a.b.c.d/n`) a LAN address is a finding inside; outside them it is a `lan-example`.
    pub networks: Vec<String>,
    /// A file outside git with more networks, one per line (`#` and blank lines skipped); the CLI reads it.
    pub networks_file: Option<String>,
    /// Names that are placeholders, not people: a home folder, a volume or an entity id named so is no finding.
    pub placeholders: Vec<String>,
    /// Regexes an email address may match to be no finding, beside the built-in public shapes (documentation
    /// domains, file names, git remotes, no-reply senders, one-letter stand-ins).
    pub email_allow: Vec<String>,
    /// Regexes for names that must not survive the transformations (legacy names), in order: a span one claims is
    /// not counted again. Lookaround is allowed.
    pub leftover: Vec<String>,
    /// Regexes for names that are kept (a signing identity's label): their spans are claimed before `leftover`.
    pub leftover_keep: Vec<String>,
    /// Globs of the files whose text is read for `leftover` (default: every file).
    pub leftover_paths: Vec<String>,
    /// Regexes a line must match to be read for leftover names at all (a fast pre-filter, no lookaround; default:
    /// every line).
    pub leftover_lines: Vec<String>,
    /// Regexes for lines where a leftover name is expected (the one-release alias lines that also read the new name).
    pub leftover_ignore_lines: Vec<String>,
    /// Regexes a path must not match (`leftover-path`; default: `leftover`). A path is one finding.
    pub leftover_path: Vec<String>,
    /// Globs the export must never carry (`private-path`); such a file is not read further.
    pub private_paths: Vec<String>,
    pub licence: Option<LicenceConfig>,
    pub allow: Vec<GateAllow>,
    /// The workflow's `[[lint.allow]]` allowances (paths and private-pattern numbers), set by the CLI: a private term
    /// the lint lets through at a path is no `private-term` there either, so the lint and the gate read the private
    /// patterns alike. The `[gate]` table itself still allows no private term.
    pub private_allow: Vec<crate::LintAllow>,
    nets: Vec<(u32, u32)>,
    email_allow_regexes: Vec<Regex>,
    leftover_regexes: Vec<Matcher>,
    leftover_keep_regexes: Vec<Matcher>,
    leftover_path_regexes: Vec<Matcher>,
    leftover_line_regexes: Vec<Regex>,
    leftover_ignore_regexes: Vec<Regex>,
    leftover_scope: Option<GlobSet>,
    private_path_set: Option<GlobSet>,
}

const GATE_KEYS: &[&str] = &[
    "networks",
    "networks_file",
    "placeholders",
    "email_allow",
    "leftover",
    "leftover_keep",
    "leftover_paths",
    "leftover_lines",
    "leftover_ignore_lines",
    "leftover_path",
    "private_paths",
    "licence",
    "allow",
];

fn strings_of(tbl: &toml::Table, at: &str, key: &str) -> Result<Option<Vec<String>>, ConfigError> {
    match tbl.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ConfigError::new(format!("{at}.{key}[{i}]"), "must be a string"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ConfigError::new(
            format!("{at}.{key}"),
            "must be an array of strings",
        )),
    }
}

fn regexes(key: &str, items: &[String]) -> Result<Vec<Regex>, ConfigError> {
    items
        .iter()
        .enumerate()
        .map(|(i, p)| {
            Regex::new(p).map_err(|err| {
                ConfigError::new(format!("gate.{key}[{i}]"), format!("invalid regex: {err}"))
            })
        })
        .collect()
}

/// A leftover pattern: the linear-time `regex` engine when the pattern needs no lookaround, else fancy-regex's
/// backtracking engine (only for the patterns that need it: on every line it is two orders of magnitude slower).
#[derive(Debug, Clone)]
enum Matcher {
    Plain(Regex),
    Fancy(Box<fancy_regex::Regex>),
}

impl Matcher {
    fn is_match(&self, text: &str) -> bool {
        match self {
            Self::Plain(r) => r.is_match(text),
            Self::Fancy(r) => r.is_match(text).unwrap_or(false),
        }
    }

    fn spans(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            Self::Plain(r) => r.find_iter(text).map(|m| (m.start(), m.end())).collect(),
            Self::Fancy(r) => r
                .find_iter(text)
                .flatten()
                .map(|m| (m.start(), m.end()))
                .collect(),
        }
    }
}

fn fancy(key: &str, items: &[String]) -> Result<Vec<Matcher>, ConfigError> {
    items
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if let Ok(plain) = Regex::new(p) {
                return Ok(Matcher::Plain(plain));
            }
            fancy_regex::Regex::new(p)
                .map(|r| Matcher::Fancy(Box::new(r)))
                .map_err(|err| {
                    ConfigError::new(format!("gate.{key}[{i}]"), format!("invalid regex: {err}"))
                })
        })
        .collect()
}

impl GateConfig {
    pub fn parse(tbl: &toml::Table) -> Result<Self, ConfigError> {
        for key in tbl.keys() {
            if !GATE_KEYS.contains(&key.as_str()) {
                return Err(ConfigError::new(format!("gate.{key}"), "unknown key"));
            }
        }
        let strings = |key: &str| -> Result<Vec<String>, ConfigError> {
            Ok(strings_of(tbl, "gate", key)?.unwrap_or_default())
        };
        let networks = strings("networks")?;
        let nets = networks
            .iter()
            .enumerate()
            .map(|(i, net)| {
                parse_network(net).ok_or_else(|| {
                    ConfigError::new(format!("gate.networks[{i}]"), "must be an IPv4 a.b.c.d/n")
                })
            })
            .collect::<Result<_, _>>()?;
        let networks_file = match tbl.get("networks_file") {
            None => None,
            Some(toml::Value::String(path)) => Some(path.clone()),
            Some(_) => return Err(ConfigError::new("gate.networks_file", "must be a string")),
        };
        let email_allow = strings("email_allow")?;
        let leftover = strings("leftover")?;
        let leftover_keep = strings("leftover_keep")?;
        let leftover_paths = strings("leftover_paths")?;
        let leftover_ignore_lines = strings("leftover_ignore_lines")?;
        let leftover_lines = strings("leftover_lines")?;
        let leftover_path =
            strings_of(tbl, "gate", "leftover_path")?.unwrap_or_else(|| leftover.clone());
        let private_paths = strings("private_paths")?;
        let licence = match tbl.get("licence") {
            None => None,
            Some(toml::Value::Table(t)) => Some(LicenceConfig::parse(t)?),
            Some(_) => return Err(ConfigError::new("gate.licence", "must be a table")),
        };
        let mut allow = Vec::new();
        match tbl.get("allow") {
            None => {}
            Some(toml::Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    allow.push(parse_allow(item, &format!("gate.allow[{i}]"))?);
                }
            }
            Some(_) => return Err(ConfigError::new("gate.allow", "must be an array of tables")),
        }
        let optional_set = |globs: &[String], key: &str| -> Result<Option<GlobSet>, ConfigError> {
            if globs.is_empty() {
                Ok(None)
            } else {
                globset(globs, key).map(Some)
            }
        };
        Ok(Self {
            email_allow_regexes: regexes("email_allow", &email_allow)?,
            leftover_regexes: fancy("leftover", &leftover)?,
            leftover_keep_regexes: fancy("leftover_keep", &leftover_keep)?,
            leftover_path_regexes: fancy("leftover_path", &leftover_path)?,
            leftover_ignore_regexes: regexes("leftover_ignore_lines", &leftover_ignore_lines)?,
            leftover_line_regexes: regexes("leftover_lines", &leftover_lines)?,
            leftover_lines,
            leftover_scope: optional_set(&leftover_paths, "gate.leftover_paths")?,
            private_path_set: optional_set(&private_paths, "gate.private_paths")?,
            leftover_ignore_lines,
            leftover_keep,
            leftover_paths,
            leftover_path,
            private_paths,
            networks,
            networks_file,
            placeholders: strings("placeholders")?,
            email_allow,
            leftover,
            licence,
            allow,
            private_allow: Vec::new(),
            nets,
        })
    }

    /// Adds networks read from `networks_file`; an error names the line number only (the networks are private).
    pub fn add_networks(&mut self, text: &str) -> Result<usize, String> {
        let mut added = 0;
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let net = parse_network(line)
                .ok_or_else(|| format!("line {}: not an IPv4 a.b.c.d/n", index + 1))?;
            self.nets.push(net);
            added += 1;
        }
        Ok(added)
    }

    /// True when a `[[lint.allow]]` lets private pattern `pattern` (1-based) through at `path`.
    pub fn private_allowed(&self, path: &str, pattern: usize) -> bool {
        self.private_allow
            .iter()
            .any(|allow| allow.allows(path, pattern))
    }

    fn allowed(&self, class: &str, path: &str) -> bool {
        self.allow
            .iter()
            .any(|allow| allow.class == class && allow.matcher.is_match(path))
    }

    fn inside_networks(&self, ip: u32) -> bool {
        self.nets.iter().any(|(net, mask)| ip & mask == *net)
    }

    fn placeholder(&self, name: &[u8]) -> bool {
        self.placeholders.iter().any(|p| p.as_bytes() == name)
    }

    /// A mailbox that names nobody: the documentation and example domains, a no-reply or placeholder local part, plus
    /// `email_allow`.
    fn email_public(&self, local: &[u8], domain: &[u8]) -> bool {
        let local = String::from_utf8_lossy(local).to_lowercase();
        let domain = String::from_utf8_lossy(domain).to_lowercase();
        let labels: Vec<&str> = domain.split('.').collect();
        FILE_TLDS.contains(labels.last().unwrap_or(&""))
            || local == "git"
            || domain == "github.com"
            || domain.contains("noreply")
            || local.contains("no-reply")
            || EMAIL_DOMAINS.contains(&domain.as_str())
            || EMAIL_TLDS.iter().any(|tld| domain.ends_with(tld))
            || EMAIL_DOMAINS
                .iter()
                .any(|d| domain.ends_with(&format!(".{d}")))
            || labels.iter().any(|l| l.starts_with("example"))
            || (local.len() == 1 && labels[0].len() == 1)
            || {
                let literal = format!("{local}@{domain}");
                self.email_allow_regexes
                    .iter()
                    .any(|r| r.is_match(&literal))
            }
    }
}

fn parse_allow(item: &toml::Value, at: &str) -> Result<GateAllow, ConfigError> {
    let entry = item
        .as_table()
        .ok_or_else(|| ConfigError::new(at, "must be a table"))?;
    for key in entry.keys() {
        if !["class", "paths", "reason"].contains(&key.as_str()) {
            return Err(ConfigError::new(format!("{at}.{key}"), "unknown key"));
        }
    }
    let class = entry
        .get("class")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| ConfigError::new(format!("{at}.class"), "is required"))?;
    if !CLASSES.contains(&class) {
        return Err(ConfigError::new(
            format!("{at}.class"),
            format!("unknown class \"{class}\""),
        ));
    }
    if class == "private-term" {
        return Err(ConfigError::new(
            format!("{at}.class"),
            "a private term is stripped or scrubbed, never allowed",
        ));
    }
    let paths: Vec<String> = entry
        .get("paths")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| ConfigError::new(format!("{at}.paths"), "is required"))?
        .iter()
        .enumerate()
        .map(|(i, p)| {
            p.as_str()
                .map(str::to_string)
                .ok_or_else(|| ConfigError::new(format!("{at}.paths[{i}]"), "must be a string"))
        })
        .collect::<Result<_, _>>()?;
    if paths.is_empty() {
        return Err(ConfigError::new(
            format!("{at}.paths"),
            "names at least one glob",
        ));
    }
    let reason = entry
        .get("reason")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| {
            ConfigError::new(
                format!("{at}.reason"),
                "is required (why the class is public there)",
            )
        })?
        .to_string();
    let matcher = globset(&paths, &format!("{at}.paths"))?;
    Ok(GateAllow {
        class: class.to_string(),
        paths,
        reason,
        matcher,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassCount {
    pub hits: usize,
    pub files: usize,
}

/// One file's count for one class: what `copybera gate --files` prints (never the matched text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFinding {
    pub path: String,
    pub class: &'static str,
    pub count: usize,
}

/// What a path that carries a private term is printed as.
pub const WITHHELD: &str = "<path withheld: private term>";

#[derive(Debug, Clone, Default)]
pub struct GateReport {
    pub files_scanned: usize,
    pub counts: BTreeMap<&'static str, ClassCount>,
    /// Every finding by file, `lan-example` included; a path that carries a private term is [`WITHHELD`].
    pub files: Vec<FileFinding>,
    /// SPDX headers and licence files by id (the census, not findings).
    pub licences: BTreeMap<String, usize>,
    pub carried_sources: usize,
}

impl GateReport {
    /// Every hit of every class but `lan-example`.
    pub fn findings(&self) -> usize {
        self.counts
            .iter()
            .filter(|(class, _)| **class != "lan-example")
            .map(|(_, count)| count.hits)
            .sum()
    }

    fn record(&mut self, path: &str, hits: BTreeMap<&'static str, usize>, withheld: bool) {
        for (class, n) in hits {
            let count = self.counts.get_mut(class).expect("every class is counted");
            count.hits += n;
            count.files += 1;
            self.files.push(FileFinding {
                path: if withheld {
                    WITHHELD.into()
                } else {
                    path.into()
                },
                class,
                count: n,
            });
        }
    }
}

/// Scans every file of `files` (and every path) by class; no origin, so no dropped-notice check.
pub fn scan(files: &BTreeMap<String, Blob>, config: &GateConfig, private: &[Regex]) -> GateReport {
    scan_with_origin(files, config, private, None)
}

/// [`scan`], plus the origin's paths for `[gate.licence] dropped_notices`.
pub fn scan_with_origin(
    files: &BTreeMap<String, Blob>,
    config: &GateConfig,
    private: &[Regex],
    origin: Option<&[String]>,
) -> GateReport {
    let mut report = GateReport {
        files_scanned: files.len(),
        counts: CLASSES
            .iter()
            .map(|class| (*class, ClassCount::default()))
            .collect(),
        ..GateReport::default()
    };
    for (path, raw) in files {
        let mut hits: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut add = |class: &'static str, n: usize| {
            if n > 0 && !config.allowed(class, path) {
                *hits.entry(class).or_default() += n;
            }
        };
        // A path that matches a pattern is never printed, allowed or not; it is a finding unless allowed.
        let withheld = private.iter().any(|r| r.is_match(path));
        let private_hit = |text: &str| {
            private
                .iter()
                .enumerate()
                .any(|(at, r)| r.is_match(text) && !config.private_allowed(path, at + 1))
        };
        add("private-term", usize::from(private_hit(path)));
        if config
            .private_path_set
            .as_ref()
            .is_some_and(|set| set.is_match(path))
        {
            add("private-path", 1);
            report.record(path, hits, withheld);
            continue;
        }
        add("audio", usize::from(audio_signature(raw).is_some()));
        let name = path.rsplit('/').next().unwrap_or(path);
        let credit = CREDIT_NAME.is_match(name);
        let data = masked(raw);
        scan_bytes(&data, config, credit, true, &mut add);
        scan_bytes(path.as_bytes(), config, credit, false, &mut add);
        // A text file is read line by line; a binary one (a NUL in its head, or not UTF-8) by its printable runs of
        // at least PRINTABLE_RUN bytes, as `strings -n 8` reads it: compressed pixels and glyph outlines spell short
        // words by chance, while a path, a host or a name a binary carries sits in a longer run.
        let private_lines = match as_text(&data) {
            Some(text) => text.lines().filter(|line| private_hit(line)).count(),
            None => printable_runs(&data).filter(|run| private_hit(run)).count(),
        };
        add("private-term", private_lines);
        add(
            "leftover-path",
            usize::from(
                config
                    .leftover_path_regexes
                    .iter()
                    .any(|r| r.is_match(path)),
            ),
        );
        let text = as_text(&data);
        if let Some(text) = text {
            if config
                .leftover_scope
                .as_ref()
                .is_none_or(|scope| scope.is_match(path))
            {
                add("leftover", leftover_hits(text, config));
            }
        }
        if let Some(licence) = &config.licence {
            let suffix = suffix_of(name);
            if licence.spdx_suffixes.iter().any(|s| s == suffix) {
                for id in spdx_ids(&data) {
                    if !licence.licence_ok(&id) {
                        add("licence", 1);
                    }
                    *report.licences.entry(id).or_default() += 1;
                }
            }
            if LICENCE_FILE.is_match(name)
                && path != "LICENSE"
                && !SOURCE_SUFFIXES.contains(&suffix)
            {
                let id = licence_of_text(text.unwrap_or(""));
                if !licence.allowed.iter().any(|a| a == id) {
                    add("licence", 1);
                }
                *report.licences.entry(format!("file:{id}")).or_default() += 1;
            }
        }
        report.record(path, hits, withheld);
    }
    if let Some(licence) = &config.licence {
        tree_licence_checks(files, config, licence, origin, &mut report);
    }
    report
}

/// The tree-wide licence checks: required notices, carried upstreams and (with the origin) dropped notices.
fn tree_licence_checks(
    files: &BTreeMap<String, Blob>,
    config: &GateConfig,
    licence: &LicenceConfig,
    origin: Option<&[String]>,
    report: &mut GateReport,
) {
    let mut extra: BTreeMap<String, BTreeMap<&'static str, usize>> = BTreeMap::new();
    let mut add = |path: &str, class: &'static str| {
        if !config.allowed(class, path) {
            *extra
                .entry(path.to_string())
                .or_default()
                .entry(class)
                .or_default() += 1;
        }
    };
    let has_dir = |dir: &str| {
        let dir = format!("{}/", dir.trim_end_matches('/'));
        files
            .range(dir.clone()..)
            .next()
            .is_some_and(|(f, _)| f.starts_with(&dir))
    };
    for need in &licence.notices {
        if !files.contains_key(need) {
            add(need, "notice");
        }
    }
    let carried = licence
        .carried
        .as_ref()
        .and_then(|manifest| files.get(manifest))
        .map(|bytes| carried_sources(bytes, &licence.carried_kinds))
        .unwrap_or_default();
    report.carried_sources = carried.len();
    for (id, paths) in &carried {
        let hit = paths
            .iter()
            .find(|p| files.contains_key(p.as_str()) || has_dir(p));
        if let Some(hit) = hit {
            if !licence.licence_ok(id) {
                add(hit, "licence");
            }
        }
    }
    if let (true, Some(origin)) = (licence.dropped_notices, origin) {
        for rel in origin {
            let Some((folder, name)) = rel.rsplit_once('/') else {
                continue;
            };
            if NOTICE_FILE.is_match(name) && !files.contains_key(rel) && has_dir(folder) {
                add(rel, "notice");
            }
        }
    }
    for (path, hits) in extra {
        report.record(&path, hits, false);
    }
}

/// The SPDX ids in the first 40 lines of the first 8 KiB (a binary head has none).
fn spdx_ids(data: &[u8]) -> Vec<String> {
    let head = &data[..data.len().min(8192)];
    if head.contains(&0) {
        return Vec::new();
    }
    let head = match std::str::from_utf8(head) {
        Ok(text) => text,
        Err(err) => std::str::from_utf8(&head[..err.valid_up_to()]).unwrap_or(""),
    };
    head.lines()
        .take(40)
        .filter_map(|line| SPDX.captures(line).map(|m| m[1].trim().to_string()))
        .collect()
}

/// The release check's byte matchers over one file's (masked) bytes (`body`) or one path. A path is not read for
/// home paths (a source folder named `home` is nobody's), LAN addresses or person entities; a credit file
/// (`credit`) is not read for emails.
fn scan_bytes(
    data: &[u8],
    config: &GateConfig,
    credit: bool,
    body: bool,
    add: &mut dyn FnMut(&'static str, usize),
) {
    let follows = |start: usize, set: &[u8]| start > 0 && set.contains(&data[start - 1]);
    if body {
        // A folder named `Users` or `home` inside a longer path (`src/Users/`, `fixtures/home/`) is nobody's home.
        let home = |regex: &BytesRegex| {
            regex
                .captures_iter(data)
                .filter(|c| {
                    let start = c.get(0).map_or(0, |m| m.start());
                    !follows(
                        start,
                        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-",
                    ) && !config.placeholder(&c[1])
                })
                .count()
        };
        let users = home(&HOME_USERS);
        let homes = home(&HOME_HOME);
        add("home-path", users + homes);
    }
    // A volume named for a placeholder word (`Data`, `Data Disk`) is nobody's.
    add(
        "volume-path",
        VOLUME
            .captures_iter(data)
            .filter(|c| {
                let name = &c[1];
                !config.placeholders.iter().any(|p| {
                    let p = p.as_bytes();
                    name.starts_with(p) && name.get(p.len()).is_none_or(|b| !WORD.contains(b))
                })
            })
            .count(),
    );
    if !credit {
        add(
            "email",
            EMAIL
                .captures_iter(data)
                .filter(|c| !ICON_DOMAIN.is_match(&c[2]) && !config.email_public(&c[1], &c[2]))
                .count(),
        );
    }
    add("secret-ref", SECRET_REF.find_iter(data).count());
    add("source-map", SOURCE_MAP.find_iter(data).count());
    add(
        "token",
        TOKEN
            .find_iter(data)
            .filter(|m| {
                let bytes = m.as_bytes();
                let guarded = bytes.starts_with(b"-----") || !follows(m.start(), WORD);
                let akia_end = !bytes.starts_with(b"AKIA")
                    || data
                        .get(m.end())
                        .is_none_or(|b| !(b.is_ascii_digit() || b.is_ascii_uppercase()));
                guarded && akia_end && !TOKEN_PLACEHOLDER.is_match(bytes)
            })
            .count(),
    );
    let lowered = data.to_ascii_lowercase();
    let mut lan_hosts = 0;
    for suffix in [&b"ts.net"[..], &b"home.arpa"[..]] {
        for i in find_word(&lowered, suffix) {
            let window = &data[i.saturating_sub(128)..i];
            if LAN_HOST_LABEL
                .find(window)
                .is_some_and(|m| m.as_bytes().ends_with(b"."))
            {
                lan_hosts += 1;
            }
        }
    }
    add("lan-host", lan_hosts);
    if body {
        let (mut inside, mut outside) = (0, 0);
        for m in LAN.find_iter(data) {
            if follows(m.start(), b"0123456789.") {
                continue;
            }
            let after = &data[m.end()..];
            if after.first().is_some_and(u8::is_ascii_digit)
                || (after.first() == Some(&b'.') && after.get(1).is_some_and(u8::is_ascii_digit))
            {
                continue;
            }
            let Some(ip) = std::str::from_utf8(m.as_bytes()).ok().and_then(parse_ipv4) else {
                continue;
            };
            if !private_or_tailnet(ip) {
                continue;
            }
            if config.inside_networks(ip) {
                inside += 1;
            } else {
                outside += 1;
            }
        }
        add("lan-address", inside);
        add("lan-example", outside);
        add(
            "person-entity",
            ENTITY
                .captures_iter(data)
                .filter(|c| !config.placeholder(&c[1]))
                .count(),
        );
    }
}

/// The leftover names on the lines of one text: keep spans first, then each leftover regex in order; a span already
/// claimed is not counted again.
fn leftover_hits(text: &str, config: &GateConfig) -> usize {
    if config.leftover_regexes.is_empty() {
        return 0;
    }
    let overlaps = |taken: &[(usize, usize)], s: usize, e: usize| {
        taken.iter().any(|(ts, te)| s < *te && *ts < e)
    };
    let mut total = 0;
    for line in text.lines() {
        if (!config.leftover_line_regexes.is_empty()
            && !config
                .leftover_line_regexes
                .iter()
                .any(|r| r.is_match(line)))
            || config
                .leftover_ignore_regexes
                .iter()
                .any(|r| r.is_match(line))
        {
            continue;
        }
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for (counted, set) in [
            (false, &config.leftover_keep_regexes),
            (true, &config.leftover_regexes),
        ] {
            for regex in set {
                for (start, end) in regex.spans(line) {
                    if start != end && !overlaps(&taken, start, end) {
                        taken.push((start, end));
                        total += usize::from(counted);
                    }
                }
            }
        }
    }
    total
}

/// The container a file's first bytes name when it is audio (no audio in a public destination, whatever the workflow
/// excludes: a recording can carry a voice). Read by signature, so a renamed clip is still caught and a text
/// file that names a sound is not.
pub fn audio_signature(bytes: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, tag: &[u8]| bytes.get(offset..offset + tag.len()) == Some(tag);
    if at(0, b"RIFF") && at(8, b"WAVE") {
        return Some("wav");
    }
    if at(0, b"FORM") && (at(8, b"AIFF") || at(8, b"AIFC")) {
        return Some("aiff");
    }
    if at(0, b"OggS") {
        return Some("ogg");
    }
    if at(0, b"fLaC") {
        return Some("flac");
    }
    if at(0, b"caff") {
        return Some("caf");
    }
    if at(0, b"ID3") {
        return Some("mp3");
    }
    if at(4, b"ftyp") && (at(8, b"M4A ") || at(8, b"M4B ")) {
        return Some("m4a");
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xF6) == 0xF0 {
        return Some("aac");
    }
    None
}

/// The shortest printable run of a binary file read for private terms.
pub const PRINTABLE_RUN: usize = 8;

/// The runs of printable ASCII (and tabs) of at least [`PRINTABLE_RUN`] bytes in `data`.
fn printable_runs(data: &[u8]) -> impl Iterator<Item = &str> {
    data.split(|b| !(b.is_ascii_graphic() || *b == b' ' || *b == b'\t'))
        .filter(|run| run.len() >= PRINTABLE_RUN)
        .filter_map(|run| std::str::from_utf8(run).ok())
}

fn as_text(data: &[u8]) -> Option<&str> {
    if data[..data.len().min(8192)].contains(&0) {
        return None;
    }
    std::str::from_utf8(data).ok()
}

fn suffix_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => "",
        Some(i) => &name[i..],
    }
}

/// The licence a licence file states: the marker that comes first in its text (a file that grants MIT and then lists
/// what the tools it runs carry names MIT).
pub fn licence_of_text(text: &str) -> &'static str {
    let head: String = text.chars().take(6000).collect::<String>().to_lowercase();
    let low = head.split_whitespace().collect::<Vec<_>>().join(" ");
    LICENCE_TEXT
        .iter()
        .filter_map(|(marker, id)| low.find(marker).map(|pos| (pos, *id)))
        .min()
        .map_or("unknown", |(_, id)| id)
}

/// The carried upstreams of a notices manifest: (licence, paths) for each `[[source]]` of a carried kind.
fn carried_sources(bytes: &[u8], kinds: &[String]) -> Vec<(String, Vec<String>)> {
    let Some(doc) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
    else {
        return Vec::new();
    };
    doc.get("source")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .filter(|s| {
            let kind = s.get("kind").and_then(toml::Value::as_str).unwrap_or("");
            kinds.iter().any(|k| k == kind)
        })
        .map(|s| {
            let licence = s
                .get("licence")
                .and_then(toml::Value::as_str)
                .unwrap_or("unknown");
            let paths = s
                .get("paths")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect();
            (licence.to_string(), paths)
        })
        .collect()
}

/// Offsets of `term` (lower case) in `lowered` as a whole word (ASCII alphanumerics delimit).
fn find_word<'a>(lowered: &'a [u8], term: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    let mut from = 0;
    std::iter::from_fn(move || {
        while from + term.len() <= lowered.len() {
            let i = from
                + lowered[from..]
                    .windows(term.len())
                    .position(|w| w == term)?;
            from = i + 1;
            let end = i + term.len();
            if (i == 0 || !lowered[i - 1].is_ascii_alphanumeric())
                && (end >= lowered.len() || !lowered[end].is_ascii_alphanumeric())
            {
                return Some(i);
            }
        }
        None
    })
}

/// A Mach-O file's code signature blobs zeroed: the signature carries the signer's own (public) identity.
pub fn masked(data: &[u8]) -> Cow<'_, [u8]> {
    let ranges = signature_ranges(data);
    if ranges.is_empty() {
        return Cow::Borrowed(data);
    }
    let mut buf = data.to_vec();
    for (start, end) in ranges {
        let end = end.min(buf.len());
        if start < end {
            buf[start..end].fill(0);
        }
    }
    Cow::Owned(buf)
}

fn u32_at(data: &[u8], at: usize, big: bool) -> Option<u32> {
    let b: [u8; 4] = data.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(if big {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    })
}

fn signature_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let thin = |base: usize| -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        if base + 32 > data.len() {
            return found;
        }
        let magic = u32_at(data, base, false).unwrap_or(0);
        if magic != 0xFEED_FACF && magic != 0xFEED_FACE {
            return found;
        }
        let ncmds = u32_at(data, base + 16, false).unwrap_or(0);
        let mut off = base + if magic == 0xFEED_FACF { 32 } else { 28 };
        for _ in 0..ncmds {
            let (Some(cmd), Some(size)) = (u32_at(data, off, false), u32_at(data, off + 4, false))
            else {
                break;
            };
            if cmd == 0x1D && off + 16 <= data.len() {
                let dataoff = u32_at(data, off + 8, false).unwrap_or(0) as usize;
                let datasize = u32_at(data, off + 12, false).unwrap_or(0) as usize;
                found.push((base + dataoff, base + dataoff + datasize));
            }
            if size < 8 {
                break;
            }
            off += size as usize;
        }
        found
    };
    if u32_at(data, 0, true) == Some(0xCAFE_BABE) {
        let count = u32_at(data, 4, true).unwrap_or(0);
        if count > 32 {
            return Vec::new(); // a Java class file shares the magic
        }
        return (0..count as usize)
            .filter_map(|i| u32_at(data, 8 + i * 20 + 8, true))
            .flat_map(|off| thin(off as usize))
            .collect();
    }
    thin(0)
}

/// A private (RFC 1918), IETF-assignment (192.0.0.0/24) or tailnet (CGNAT, 100.64.0.0/10) address. RFC 5737's
/// documentation networks are none of these: an example written in one is what an export should hold (Python's
/// `is_private` counts TEST-NET-1, so a scrub's `192.0.2.N` would be counted as the LAN it replaced).
fn private_or_tailnet(ip: u32) -> bool {
    let in_net = |net: u32, bits: u32| ip & (u32::MAX << (32 - bits)) == net;
    in_net(0x0A00_0000, 8)
        || in_net(0xAC10_0000, 12)
        || in_net(0xC0A8_0000, 16)
        || in_net(0xC000_0000, 24)
        || in_net(0x6440_0000, 10)
}

fn parse_ipv4(text: &str) -> Option<u32> {
    let mut value = 0u32;
    let mut parts = 0;
    for part in text.split('.') {
        value = (value << 8) | u32::from(part.parse::<u8>().ok()?);
        parts += 1;
    }
    (parts == 4).then_some(value)
}

fn parse_network(text: &str) -> Option<(u32, u32)> {
    let (address, bits) = text.split_once('/')?;
    let bits: u32 = bits.parse().ok().filter(|b| *b <= 32)?;
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    Some((parse_ipv4(address)? & mask, mask))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> GateConfig {
        GateConfig::parse(&toml.parse::<toml::Table>().unwrap()).unwrap()
    }

    #[test]
    fn licence_expressions_follow_the_redistribution_list() {
        let licence = LicenceConfig::default();
        assert!(licence.licence_ok("MIT"));
        assert!(licence.licence_ok("MIT OR Apache-2.0"));
        assert!(licence.licence_ok("(MIT AND Zlib)"));
        assert!(!licence.licence_ok("GPL-3.0-only"));
        assert!(!licence.licence_ok("MIT AND GPL-3.0"));
        assert!(!licence.licence_ok("MIT + Commons Clause"));
    }

    #[test]
    fn a_licence_text_is_named_by_its_first_marker() {
        assert_eq!(
            licence_of_text("MIT License\n\nPermission is hereby granted, free of charge"),
            "mit"
        );
        assert_eq!(
            licence_of_text("\"Commons Clause\" License Condition v1.0\n\nMIT License"),
            "commons-clause"
        );
        assert_eq!(licence_of_text("all rights reserved"), "unknown");
    }

    #[test]
    fn leftover_spans_are_claimed_once_and_kept_names_first() {
        let c = config(
            "leftover = ['(?<![A-Za-z0-9_])oldname-[a-z]+', 'oldname']\n\
             leftover_keep = ['\\.config/acme/codesign']\nleftover_ignore_lines = ['(?i)rebrand']\n",
        );
        assert_eq!(leftover_hits("run oldname-stt now", &c), 1);
        assert_eq!(
            leftover_hits("x-oldname-stt", &c),
            1,
            "the lookbehind refuses; the catch-all counts"
        );
        assert_eq!(leftover_hits("oldname-stt beside rebrand-stt", &c), 0);
        let k = config(
            "leftover = ['\\.config/acme[a-z/]*']\nleftover_keep = ['\\.config/acme/codesign']\n",
        );
        assert_eq!(leftover_hits("~/.config/acme/codesign", &k), 0);
        assert_eq!(leftover_hits("~/.config/acme/cache", &k), 1);
    }

    fn tree(files: &[(&str, &[u8])]) -> BTreeMap<String, Blob> {
        files
            .iter()
            .map(|(path, bytes)| ((*path).to_string(), Blob::from(*bytes)))
            .collect()
    }

    #[test]
    fn private_terms_follow_the_lint_allowances() {
        let mut gate = config("");
        gate.private_allow =
            vec![crate::LintAllow::from_parts(vec!["**/*.rs".into()], vec![2]).unwrap()];
        let private = [
            Regex::new("quokkahouse").unwrap(),
            Regex::new(r"\.local\b").unwrap(),
        ];
        let files = tree(&[
            ("src/a.rs", b"let x = pose.local;\n"),
            ("src/b.rs", b"// quokkahouse\nlet y = s.local;\n"),
            ("docs/c.md", b"see pose.local\n"),
        ]);
        let report = scan(&files, &gate, &private);
        assert_eq!(report.counts["private-term"].hits, 2);
        let rows: Vec<_> = report
            .files
            .iter()
            .filter(|f| f.class == "private-term")
            .map(|f| (f.path.as_str(), f.count))
            .collect();
        assert_eq!(rows, [("docs/c.md", 1), ("src/b.rs", 1)]);
    }

    #[test]
    fn a_binary_is_read_for_private_terms_in_its_printable_runs() {
        let gate = config("");
        let private = [Regex::new(r"\bzed\b").unwrap()];
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\x01\x02 zed \xff\x10".to_vec();
        png.extend_from_slice(b"\0tEXt\0Author: zed the operator\0\x07");
        let report = scan(&tree(&[("shot.png", &png)]), &gate, &private);
        assert_eq!(
            report.counts["private-term"].hits, 1,
            "the short run is a collision, the long one is text"
        );
        let text = scan(&tree(&[("a.txt", b"zed\n")]), &gate, &private);
        assert_eq!(
            text.counts["private-term"].hits, 1,
            "a text file is read line by line, every length"
        );
    }

    #[test]
    fn documentation_addresses_are_no_lan_example() {
        let gate = config("");
        let files = tree(&[(
            "src/a.rs",
            b"ok 192.0.2.7 198.51.100.7 203.0.113.7; lan 10.9.9.9 192.168.1.2 100.64.0.1\n",
        )]);
        assert_eq!(scan(&files, &gate, &[]).counts["lan-example"].hits, 3);
    }

    #[test]
    fn a_mach_o_signature_is_masked() {
        let mut bin = vec![0u8; 256];
        bin[..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
        bin[16..20].copy_from_slice(&1u32.to_le_bytes());
        bin[32..36].copy_from_slice(&0x1Du32.to_le_bytes());
        bin[36..40].copy_from_slice(&16u32.to_le_bytes());
        bin[40..44].copy_from_slice(&200u32.to_le_bytes());
        bin[44..48].copy_from_slice(&40u32.to_le_bytes());
        bin[200..216].copy_from_slice(b"/Users/signer/xx");
        assert!(!masked(&bin).windows(7).any(|w| w == b"/Users/"));
    }
}
