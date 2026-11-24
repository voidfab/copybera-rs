use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use copybera_rules::{Blob, Commit, PathFilter};

pub struct Author<'a> {
    pub name: &'a str,
    pub email: &'a str,
    pub date: Option<&'a str>,
}

pub struct Git {
    pub home: Option<PathBuf>,
}

/// The files and modes (other than `100644`) of a tree to write.
pub struct TreeRef<'a> {
    pub files: &'a BTreeMap<String, Blob>,
    pub modes: &'a BTreeMap<String, String>,
}

pub struct LoadedTrees {
    pub trees: Vec<BTreeMap<String, Blob>>,
    /// Each tree's modes other than `100644`, by path.
    pub modes: Vec<BTreeMap<String, String>>,
    pub blobs_read: usize,
    pub files_read: usize,
}

impl Git {
    pub fn new(home: Option<PathBuf>) -> Self {
        Self { home }
    }

    pub fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        self.base_env(&mut cmd);
        let output = cmd
            .output()
            .with_context(|| format!("git {} in {}", args.join(" "), repo.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "git {} failed in {}: {stderr}",
                args.join(" "),
                repo.display()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    }

    /// `run`'s raw stdout: not trimmed, not decoded.
    fn run_bytes(&self, repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        self.base_env(&mut cmd);
        let output = cmd
            .output()
            .with_context(|| format!("git {} in {}", args.join(" "), repo.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "git {} failed in {}: {stderr}",
                args.join(" "),
                repo.display()
            ));
        }
        Ok(output.stdout)
    }

    fn base_env(&self, cmd: &mut Command) {
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        if let Some(home) = &self.home {
            cmd.env("HOME", home);
        }
    }

    pub fn is_repo(&self, path: &Path) -> bool {
        self.run(path, &["rev-parse", "--git-dir"]).is_ok()
    }

    pub fn is_bare(&self, path: &Path) -> bool {
        self.run(path, &["rev-parse", "--is-bare-repository"])
            .map(|s| s == "true")
            .unwrap_or(false)
    }

    pub fn is_dirty(&self, path: &Path) -> Result<bool> {
        if !self.is_repo(path) {
            return Ok(false);
        }
        if self.is_bare(path) {
            return Ok(false);
        }
        let out = self.run(path, &["status", "--porcelain"])?;
        Ok(!out.is_empty())
    }

    /// Whether a tracked file is modified or staged; untracked files are not counted (an origin checkout carries
    /// its user's own scratch files, which a migration never reads: it walks commits).
    pub fn has_tracked_changes(&self, path: &Path) -> Result<bool> {
        if !self.is_repo(path) {
            return Ok(false);
        }
        if self.is_bare(path) {
            return Ok(false);
        }
        let out = self.run(path, &["status", "--porcelain", "--untracked-files=no"])?;
        Ok(!out.is_empty())
    }

    pub fn rev_parse(&self, repo: &Path, rev: &str) -> Result<String> {
        self.run(repo, &["rev-parse", "--verify", rev])
    }

    pub fn rev_parse_optional(&self, repo: &Path, rev: &str) -> Result<Option<String>> {
        match self.rev_parse(repo, rev) {
            Ok(sha) => Ok(Some(sha)),
            Err(_) => Ok(None),
        }
    }

    pub fn rev_list(&self, repo: &Path, since: Option<&str>, until: &str) -> Result<Vec<String>> {
        let range = match since {
            Some(since) => format!("{since}..{until}"),
            None => until.to_string(),
        };
        self.rev_list_range(repo, &range, true)
    }

    pub fn rev_list_range(
        &self,
        repo: &Path,
        range: &str,
        first_parent: bool,
    ) -> Result<Vec<String>> {
        let mut args = vec!["rev-list", "--reverse"];
        if first_parent {
            args.push("--first-parent");
        }
        args.push(range);
        let out = self.run(repo, &args)?;
        if out.is_empty() {
            return Ok(Vec::new());
        }
        Ok(out.lines().map(|l| l.trim().to_string()).collect())
    }

    pub fn load_commit(&self, repo: &Path, id: &str) -> Result<Commit> {
        let mut commit = self.load_commit_header(repo, id)?;
        (commit.files, commit.modes) = self.load_tree_with_modes(repo, id)?;
        Ok(commit)
    }

    pub fn load_commit_header(&self, repo: &Path, id: &str) -> Result<Commit> {
        let header = self.run(repo, &["log", "-1", "--format=%an%n%ae%n%aI%n%B", id])?;
        let mut lines = header.splitn(4, '\n');
        let author_name = lines.next().unwrap_or("").to_string();
        let author_email = lines.next().unwrap_or("").to_string();
        let author_date = lines.next().unwrap_or("").to_string();
        let message = lines.next().unwrap_or("").trim_end().to_string();
        Ok(Commit {
            id: id.to_string(),
            message,
            author_name,
            author_email,
            author_date: if author_date.is_empty() {
                None
            } else {
                Some(author_date)
            },
            files: BTreeMap::new(),
            modes: BTreeMap::new(),
        })
    }

    /// Every commit's header (author, date, message) from one `git log --stdin --no-walk=unsorted` stream, in the
    /// order of `ids`: what `load_commit_header` reads per commit, with one spawn for a whole walk instead of one
    /// per commit (thousands of spawns on a long history).
    pub fn load_commit_headers(&self, repo: &Path, ids: &[String]) -> Result<Vec<Commit>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let args = [
            "log",
            "--stdin",
            "--no-walk=unsorted",
            "--format=%H%x00%an%x00%ae%x00%aI%x00%B%x00",
        ];
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        self.base_env(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("git {} in {}", args.join(" "), repo.display()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("git log --stdin: stdin closed"))?;
        let feed: String = ids.iter().map(|id| format!("{id}\n")).collect();
        let writer = std::thread::spawn(move || stdin.write_all(feed.as_bytes()));
        let output = child.wait_with_output().context("git log --stdin: wait")?;
        writer
            .join()
            .map_err(|_| anyhow!("git log --stdin: the writer panicked"))?
            .context("git log --stdin: write the ids")?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed in {}: {}",
                args.join(" "),
                repo.display(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let headers = parse_commit_headers(&String::from_utf8_lossy(&output.stdout));
        if headers.len() != ids.len()
            || headers.iter().zip(ids).any(|(header, id)| &header.id != id)
        {
            return Err(anyhow!(
                "git log --stdin: {} headers for {} commits, or not in their order",
                headers.len(),
                ids.len()
            ));
        }
        Ok(headers)
    }

    /// A tree's files and its modes other than `100644`.
    #[allow(clippy::type_complexity)]
    pub fn load_tree_with_modes(
        &self,
        repo: &Path,
        treeish: &str,
    ) -> Result<(BTreeMap<String, Blob>, BTreeMap<String, String>)> {
        let treeishes = vec![treeish.to_string()];
        let loaded = self.load_trees_with_filter_once(repo, &treeishes, None)?;
        let files = loaded.trees.into_iter().next();
        let modes = loaded.modes.into_iter().next();
        Ok((
            files.expect("one requested tree produces one loaded tree"),
            modes.unwrap_or_default(),
        ))
    }

    /// Every blob path of `treeish` (no contents): the gate's dropped-notice check reads the origin's paths only.
    pub fn list_paths(&self, repo: &Path, treeish: &str) -> Result<Vec<String>> {
        let listing = self.run_bytes(repo, &["ls-tree", "-r", "-z", "--name-only", treeish])?;
        Ok(listing
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect())
    }

    /// Loads every requested tree using one object-id cache and one `cat-file --batch` process. The trees share the
    /// cache's bytes: a blob that several trees carry is held once.
    pub fn load_trees_filtered_once(
        &self,
        repo: &Path,
        treeishes: &[String],
        filter: &PathFilter,
    ) -> Result<LoadedTrees> {
        self.load_trees_with_filter_once(repo, treeishes, Some(filter))
    }

    fn load_trees_with_filter_once(
        &self,
        repo: &Path,
        treeishes: &[String],
        filter: Option<&PathFilter>,
    ) -> Result<LoadedTrees> {
        let mut entries = Vec::with_capacity(treeishes.len());
        let mut tree_modes = Vec::with_capacity(treeishes.len());
        let mut objects = BTreeSet::new();
        for treeish in treeishes {
            // `-z`: without it git quotes a path holding a non-ASCII byte, a quote, a backslash or a tab, and the
            // export wrote the quoted form as a new path.
            let listing = self.run_bytes(repo, &["ls-tree", "-r", "-z", treeish])?;
            let mut tree = Vec::new();
            let mut modes = BTreeMap::new();
            for entry in listing.split(|byte| *byte == 0) {
                let entry = String::from_utf8_lossy(entry);
                let Some((meta, path)) = entry.split_once('\t') else {
                    continue;
                };
                let mut parts = meta.split_whitespace();
                let mode = parts.next().unwrap_or("");
                let kind = parts.next().unwrap_or("");
                let object = parts.next().unwrap_or("");
                if kind == "blob" && filter.is_none_or(|filter| filter.allows(path)) {
                    if mode != "100644" {
                        modes.insert(path.to_string(), mode.to_string());
                    }
                    let object = object.to_string();
                    objects.insert(object.clone());
                    tree.push((path.to_string(), object));
                }
            }
            entries.push(tree);
            tree_modes.push(modes);
        }
        let files_read = entries.iter().map(Vec::len).sum();
        let objects: Vec<String> = objects.into_iter().collect();
        let blobs_read = objects.len();
        let cache: BTreeMap<String, Blob> = objects
            .iter()
            .cloned()
            .zip(self.cat_objects(repo, &objects)?)
            .collect();
        let trees = entries
            .into_iter()
            .map(|tree| {
                tree.into_iter()
                    .map(|(path, object)| {
                        let bytes = cache
                            .get(&object)
                            .expect("every requested object was read")
                            .clone();
                        (path, bytes)
                    })
                    .collect()
            })
            .collect();
        Ok(LoadedTrees {
            trees,
            modes: tree_modes,
            blobs_read,
            files_read,
        })
    }

    /// Every object's bytes, read from one `cat-file --batch` as it streams: one blob is copied at a time, not the
    /// whole window's output and then each blob again.
    fn cat_objects(&self, repo: &Path, objects: &[String]) -> Result<Vec<Blob>> {
        if objects.is_empty() {
            return Ok(Vec::new());
        }

        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(["cat-file", "--batch"]);
        self.base_env(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("git cat-file --batch")?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("git cat-file --batch: stdin closed"))?;
        let expected = objects.len();
        let objects = objects.to_vec();
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            let mut stdin = std::io::BufWriter::new(&mut stdin);
            for object in objects {
                writeln!(stdin, "{object}")?;
            }
            stdin.flush()
        });
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("git cat-file --batch: stdout closed"))?;
        let blobs = read_batch_blobs(BufReader::with_capacity(1 << 16, stdout), expected);
        let result = child
            .wait_with_output()
            .context("git cat-file --batch: wait")?;
        writer
            .join()
            .map_err(|_| anyhow!("git cat-file --batch: write thread panicked"))?
            .context("git cat-file --batch: write object ids")?;
        if !result.status.success() {
            return Err(anyhow!(
                "git cat-file --batch failed: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        blobs
    }

    pub fn is_ancestor(&self, repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .args(["merge-base", "--is-ancestor", ancestor, descendant]);
        self.base_env(&mut cmd);
        let output = cmd.output().with_context(|| {
            format!(
                "git merge-base --is-ancestor {ancestor} {descendant} in {}",
                repo.display()
            )
        })?;
        Ok(output.status.success())
    }

    pub fn remotes(&self, repo: &Path) -> Result<Vec<String>> {
        let output = self.run(repo, &["remote"])?;
        Ok(output.lines().map(str::to_owned).collect())
    }

    pub fn copybera_rev_id(&self, repo: &Path) -> Result<Option<String>> {
        if !self.is_repo(repo) {
            return Ok(None);
        }
        let log = match self.run(
            repo,
            &[
                "log",
                "-1",
                "--format=%B",
                "--grep=Copybera-RevId",
                "--grep=GitOrigin-RevId",
            ],
        ) {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };
        Ok(parse_origin_rev_id(&log))
    }

    /// The ref HEAD names, or `None` for a detached HEAD.
    pub fn symbolic_head(&self, repo: &Path) -> Result<Option<String>> {
        match self.run(repo, &["symbolic-ref", "-q", "HEAD"]) {
            Ok(name) if !name.trim().is_empty() => Ok(Some(name.trim().to_string())),
            _ => Ok(None),
        }
    }

    pub fn set_symbolic_head(&self, repo: &Path, name: &str) -> Result<()> {
        self.run(repo, &["symbolic-ref", "HEAD", name])?;
        Ok(())
    }

    pub fn update_ref(&self, repo: &Path, name: &str, sha: &str) -> Result<()> {
        self.run(repo, &["update-ref", name, sha])?;
        Ok(())
    }

    pub fn write_commit(
        &self,
        repo: &Path,
        parent: Option<&str>,
        tree: TreeRef<'_>,
        message: &str,
        author: Author<'_>,
        committer: Author<'_>,
    ) -> Result<String> {
        let index_dir = tempfile::TempDir::new().context("temp index directory")?;
        let index = index_dir.path().join("index");
        self.run_index(repo, &index, &["read-tree", "--empty"])?;
        // One fast-import for every blob and one update-index for every entry, not two
        // processes per file.
        let blobs = self.write_blobs(repo, tree.files.values(), index_dir.path())?;
        let mut entries = Vec::new();
        for ((path, _), blob) in tree.files.iter().zip(&blobs) {
            let mode = tree.modes.get(path).map_or("100644", String::as_str);
            entries.extend_from_slice(format!("{mode} {blob}\t").as_bytes());
            entries.extend_from_slice(path.as_bytes());
            entries.push(0);
        }
        self.run_stdin(
            repo,
            Some(&index),
            &["update-index", "-z", "--index-info"],
            &mut |stdin| stdin.write_all(&entries),
        )?;
        let tree = self.run_index(repo, &index, &["write-tree"])?;
        self.commit_tree(repo, &tree, parent, message, author, committer)
    }

    /// A commit's author and committer, one `name <email>` line each, for the leak lint.
    pub fn identities(&self, repo: &Path, id: &str) -> Result<String> {
        self.run(repo, &["log", "-1", "--format=%an <%ae>%n%cn <%ce>", id])
    }

    fn run_index(&self, repo: &Path, index: &Path, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        self.base_env(&mut cmd);
        cmd.env("GIT_INDEX_FILE", index);
        let output = cmd.output().with_context(|| {
            format!(
                "git {} in {} (index {})",
                args.join(" "),
                repo.display(),
                index.display()
            )
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "git {} failed in {}: {stderr}",
                args.join(" "),
                repo.display()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    }

    /// Writes every blob through one `git fast-import` and returns their ids in order (read from its marks file).
    fn write_blobs<'a>(
        &self,
        repo: &Path,
        blobs: impl Iterator<Item = &'a Blob>,
        scratch: &Path,
    ) -> Result<Vec<String>> {
        let blobs: Vec<&Blob> = blobs.collect();
        if blobs.is_empty() {
            return Ok(Vec::new());
        }
        let marks = scratch.join("marks");
        let export = format!("--export-marks={}", marks.display());
        self.run_stdin(
            repo,
            None,
            &["fast-import", "--quiet", "--done", &export],
            &mut |stdin| {
                let mut stdin = std::io::BufWriter::new(stdin);
                for (index, bytes) in blobs.iter().enumerate() {
                    write!(stdin, "blob\nmark :{}\ndata {}\n", index + 1, bytes.len())?;
                    stdin.write_all(bytes)?;
                    stdin.write_all(b"\n")?;
                }
                stdin.write_all(b"done\n")?;
                stdin.flush()
            },
        )?;
        let text = std::fs::read_to_string(&marks).context("git fast-import: read marks")?;
        let mut ids = vec![String::new(); blobs.len()];
        for line in text.lines() {
            let (mark, id) = line
                .strip_prefix(':')
                .and_then(|rest| rest.split_once(' '))
                .ok_or_else(|| anyhow!("git fast-import: unexpected marks line"))?;
            let mark: usize = mark
                .parse()
                .map_err(|_| anyhow!("git fast-import: unexpected mark"))?;
            let slot = mark
                .checked_sub(1)
                .and_then(|at| ids.get_mut(at))
                .ok_or_else(|| anyhow!("git fast-import: mark {mark} out of range"))?;
            *slot = id.to_string();
        }
        if ids.iter().any(String::is_empty) {
            return Err(anyhow!("git fast-import: a blob has no mark"));
        }
        Ok(ids)
    }

    /// Runs git with `feed` writing its stdin (stdout discarded), optionally against a temporary index.
    fn run_stdin(
        &self,
        repo: &Path,
        index: Option<&Path>,
        args: &[&str],
        feed: &mut dyn FnMut(&mut std::process::ChildStdin) -> std::io::Result<()>,
    ) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        self.base_env(&mut cmd);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("git {} in {}", args.join(" "), repo.display()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("git {}: stdin closed", args.join(" ")))?;
        let fed = feed(&mut stdin);
        drop(stdin);
        let output = child
            .wait_with_output()
            .with_context(|| format!("git {}: wait", args.join(" ")))?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed in {}: {}",
                args.join(" "),
                repo.display(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        fed.with_context(|| format!("git {}: write stdin", args.join(" ")))
    }

    fn commit_tree(
        &self,
        repo: &Path,
        tree: &str,
        parent: Option<&str>,
        message: &str,
        author: Author<'_>,
        committer: Author<'_>,
    ) -> Result<String> {
        // The committer is the workflow's identity, never the local git config: a public destination must not carry
        // the name or address of whoever ran the export.
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).arg("commit-tree").arg(tree);
        if let Some(parent) = parent {
            cmd.arg("-p").arg(parent);
        }
        // The message goes on stdin: a squash of a long history joins thousands of subjects, and Linux refuses one
        // argument over 128 KiB (E2BIG at commit-tree).
        self.base_env(&mut cmd);
        cmd.env("GIT_AUTHOR_NAME", author.name);
        cmd.env("GIT_AUTHOR_EMAIL", author.email);
        if let Some(date) = author.date {
            cmd.env("GIT_AUTHOR_DATE", date);
        }
        cmd.env("GIT_COMMITTER_NAME", committer.name);
        cmd.env("GIT_COMMITTER_EMAIL", committer.email);
        if let Some(date) = committer.date {
            cmd.env("GIT_COMMITTER_DATE", date);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("git commit-tree")?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("git commit-tree: stdin closed"))?;
        let fed = stdin.write_all(message.as_bytes());
        drop(stdin);
        let output = child.wait_with_output().context("git commit-tree: wait")?;
        fed.context("git commit-tree: write the message")?;
        if !output.status.success() {
            return Err(anyhow!(
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    }
}

/// The commits of a `git log --format=%H%x00%an%x00%ae%x00%aI%x00%B%x00` stream: five NUL-ended fields per commit,
/// each commit after the first led by the format's newline. The fields are read as `load_commit_header` reads them.
fn parse_commit_headers(text: &str) -> Vec<Commit> {
    let mut fields = text.split('\0');
    let mut commits = Vec::new();
    while let Some(id) = fields.next() {
        let id = id.trim_start_matches('\n');
        if id.is_empty() {
            break;
        }
        let mut next = || fields.next().unwrap_or("").to_string();
        let (author_name, author_email, author_date, message) = (next(), next(), next(), next());
        commits.push(Commit {
            id: id.to_string(),
            message: message.trim_end().to_string(),
            author_name,
            author_email,
            author_date: if author_date.is_empty() {
                None
            } else {
                Some(author_date)
            },
            files: BTreeMap::new(),
            modes: BTreeMap::new(),
        });
    }
    commits
}

/// The blobs of a `cat-file --batch` stream, in order: a header line (`<oid> blob <size>`), the bytes, a newline.
fn read_batch_blobs(mut output: impl BufRead, expected: usize) -> Result<Vec<Blob>> {
    let mut blobs = Vec::with_capacity(expected);
    let mut header = Vec::new();
    for _ in 0..expected {
        header.clear();
        output
            .read_until(b'\n', &mut header)
            .context("git cat-file --batch: read blob header")?;
        if header.pop() != Some(b'\n') {
            return Err(anyhow!("git cat-file --batch: missing blob header"));
        }
        let header =
            std::str::from_utf8(&header).context("git cat-file --batch: invalid blob header")?;
        let mut fields = header.split_ascii_whitespace();
        let object = fields.next().unwrap_or("");
        let kind = fields.next().unwrap_or("");
        let size = fields
            .next()
            .ok_or_else(|| anyhow!("git cat-file --batch: malformed header {header:?}"))?
            .parse::<usize>()
            .with_context(|| format!("git cat-file --batch: malformed header {header:?}"))?;
        if object.is_empty() || kind != "blob" || fields.next().is_some() {
            return Err(anyhow!("git cat-file --batch: malformed header {header:?}"));
        }
        let mut bytes = vec![0; size + 1];
        output
            .read_exact(&mut bytes)
            .map_err(|_| anyhow!("git cat-file --batch: truncated blob {object}"))?;
        if bytes.pop() != Some(b'\n') {
            return Err(anyhow!("git cat-file --batch: truncated blob {object}"));
        }
        blobs.push(Blob::from(bytes));
    }
    let mut rest = [0u8; 1];
    if output
        .read(&mut rest)
        .context("git cat-file --batch: read stdout")?
        != 0
    {
        return Err(anyhow!("git cat-file --batch: unexpected extra output"));
    }
    Ok(blobs)
}

fn parse_origin_rev_id(message: &str) -> Option<String> {
    for line in message.lines() {
        for prefix in ["Copybera-RevId:", "GitOrigin-RevId:"] {
            if let Some(value) = line.strip_prefix(prefix) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}
