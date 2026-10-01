use std::{
    collections::HashMap,
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail};
use ignore::gitignore::GitignoreBuilder;
use super_types::{CommitRef, MAX_SOURCE_FILE_BYTES, SourceFile, SourceManifest, source_sha256, source_tree_sha256};
use tempfile::TempDir;

#[derive(Debug, Clone)]
pub struct GitRepository {
    pub root: PathBuf,
    pub common_git_dir: PathBuf,
    pub name: String,
}

#[derive(Debug)]
pub struct TemporaryCommit {
    pub commit: CommitRef,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeSnapshot {
    pub head: CommitRef,
    pub tree: String,
    index_sha256: String,
}

#[derive(Debug)]
pub struct PreparedFix {
    pub before: WorktreeSnapshot,
    pub source_changed: bool,
    pub apply_error: Option<String>,
}

#[derive(Debug)]
pub struct SourceSnapshot {
    pub manifest: SourceManifest,
    pub source_hashes: HashMap<String, String>,
    pub excluded: Vec<String>,
    content_paths: HashMap<String, String>,
}

impl SourceSnapshot {
    pub fn content_path(&self, sha256: &str) -> Option<&str> {
        self.content_paths.get(sha256).map(String::as_str)
    }
}

#[derive(Debug)]
struct TreeEntry {
    path: String,
    oid: String,
    mode: String,
    kind: String,
    bytes: Option<u64>,
}

struct TreeFile {
    path: String,
    oid: String,
    bytes: u64,
    executable: bool,
}

impl GitRepository {
    pub fn discover(path: &Path) -> Result<Self> {
        let root = run_git_at(path, ["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(root.trim()).canonicalize()?;
        let common = run_git(&root, ["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let common_git_dir = PathBuf::from(common.trim());
        let name = root
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("repository")
            .to_owned();
        Ok(Self {
            root,
            common_git_dir,
            name,
        })
    }

    pub fn head(&self) -> Result<CommitRef> {
        self.resolve_commit("HEAD")
    }

    pub fn has_commits(&self) -> Result<bool> {
        let status = git_command(&self.root)
            .args(["rev-parse", "--verify", "HEAD^{commit}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("could not inspect the repository's HEAD")?;
        Ok(status.success())
    }

    pub fn resolve_commit(&self, revision: &str) -> Result<CommitRef> {
        let expression = format!("{revision}^{{commit}}");
        let oid = run_git(&self.root, ["rev-parse", "--verify", &expression])?;
        let branch = if revision == "HEAD" {
            command_output(git_command(&self.root).args(["symbolic-ref", "--quiet", "--short", "HEAD"]))
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        } else {
            None
        };
        let oid = oid.trim().to_owned();
        let parents = run_git(&self.root, ["rev-list", "--parents", "-n", "1", &oid])?;
        let parent_oid = parents.split_whitespace().nth(1).map(str::to_owned);
        Ok(CommitRef {
            oid,
            branch,
            temporary: false,
            parent_oid,
        })
    }

    pub fn is_dirty(&self) -> Result<bool> {
        Ok(!self.status_snapshot()?.is_empty())
    }

    /// Capture working files using an isolated index, without creating a commit or moving refs.
    /// Include all Git-visible paths for race/scope checks; `source_snapshot` applies .superignore exclusions.
    pub fn worktree_snapshot(&self) -> Result<WorktreeSnapshot> {
        for _ in 0..3 {
            let temporary = TempDir::new().context("could not create an isolated Git index")?;
            let head = self.head()?;
            let index_sha256 = self.index_sha256()?;
            let first = self.write_worktree_tree(&temporary.path().join("index-1"), true)?;
            let tree = self.write_worktree_tree(&temporary.path().join("index-2"), true)?;
            if first == tree && self.head()?.oid == head.oid && self.index_sha256()? == index_sha256 {
                return Ok(WorktreeSnapshot {
                    head,
                    tree,
                    index_sha256,
                });
            }
        }
        bail!("working files or staging kept changing while super read them; stop the edits and retry")
    }

    pub fn prepare_fix(&self, base_tree_sha256: &str, diff: &str) -> Result<PreparedFix> {
        let before = self.worktree_snapshot()?;
        let source = self.source_snapshot(&before.tree, &HashMap::new())?;
        let apply_error = self.check_fix_diff(diff).err().map(|error| format!("{error:#}"));
        Ok(PreparedFix {
            before,
            source_changed: source.manifest.tree_sha256 != base_tree_sha256,
            apply_error,
        })
    }

    fn check_fix_diff(&self, diff: &str) -> Result<()> {
        if diff.is_empty() || diff.len() > 512 * 1024 || diff.contains('\0') {
            bail!("the proposed fix is empty or exceeds the 512 KiB patch limit")
        }
        if diff.contains("GIT binary patch") || diff.contains("Submodule ") {
            bail!("binary and submodule patches cannot be applied by super")
        }
        validate_fix_diff_paths(diff)?;
        run_git_with_stdin(
            &self.root,
            [
                "-c",
                "apply.ignoreWhitespace=no",
                "apply",
                "--recount",
                "--check",
                "--whitespace=error",
                "-",
            ],
            diff.as_bytes(),
        )
        .context("the proposed fix does not apply cleanly to your working files")
    }

    pub fn ensure_fix_unchanged(&self, before: &WorktreeSnapshot) -> Result<()> {
        let current = self.worktree_snapshot()?;
        if current.head.oid != before.head.oid
            || current.tree != before.tree
            || current.index_sha256 != before.index_sha256
        {
            bail!(
                "the working files, staging, or HEAD changed after the fix preview\n  No fix was applied. Run the fix command again to review the current source."
            )
        }
        Ok(())
    }

    /// Apply to working files only. Git checks every hunk before writing; no --reject, --index,
    /// three-way fallback, or whitespace/context relaxation may bypass the previewed patch.
    pub fn apply_fix_diff(&self, before: &WorktreeSnapshot, diff: &str) -> Result<()> {
        self.ensure_fix_unchanged(before)?;
        self.check_fix_diff(diff)?;
        run_git_with_stdin(
            &self.root,
            [
                "-c",
                "apply.ignoreWhitespace=no",
                "apply",
                "--recount",
                "--whitespace=error",
                "-",
            ],
            diff.as_bytes(),
        )
        .context("Git could not apply the proposed fix")?;
        Ok(())
    }

    pub fn edits_since(&self, before: &WorktreeSnapshot) -> Result<std::collections::BTreeSet<String>> {
        let after = self.worktree_snapshot()?;
        if after.head.oid != before.head.oid || after.index_sha256 != before.index_sha256 {
            bail!(
                "Git HEAD or staging changed during fix application\n  Inspect the changes before continuing; super has not restored or committed anything."
            )
        }
        let paths = run_git_bytes(
            &self.root,
            [
                "diff-tree",
                "--no-commit-id",
                "--no-renames",
                "--name-only",
                "-r",
                "-z",
                &before.tree,
                &after.tree,
            ],
        )?;
        paths
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.to_vec()).context("Git returned a non-UTF-8 path"))
            .collect()
    }

    fn index_sha256(&self) -> Result<String> {
        Ok(source_sha256(&run_git_bytes(
            &self.root,
            ["ls-files", "--stage", "-z"],
        )?))
    }

    pub fn temporary_commit(&self, known: &HashMap<String, String>) -> Result<TemporaryCommit> {
        for _ in 0..3 {
            let temporary = TempDir::new().context("could not create an isolated Git index")?;
            let parent = self.head()?;
            let first_tree = self.write_worktree_tree(&temporary.path().join("index-1"), false)?;
            let second_tree = self.write_worktree_tree(&temporary.path().join("index-2"), false)?;
            if first_tree != second_tree || self.head()?.oid != parent.oid {
                continue;
            }
            let tree = second_tree;
            let fingerprint = format!("{}:{tree}", parent.oid);
            if let Some(oid) = known.get(&fingerprint)
                && git_command(&self.root)
                    .args(["cat-file", "-e", &format!("{oid}^{{commit}}")])
                    .status()
                    .is_ok_and(|status| status.success())
            {
                return Ok(TemporaryCommit {
                    commit: CommitRef {
                        oid: oid.clone(),
                        branch: None,
                        temporary: true,
                        parent_oid: Some(parent.oid),
                    },
                    fingerprint,
                });
            }

            let oid = create_commit_object(&self.root, &tree, &parent.oid)?;
            let reference = format!("refs/super/commits/{oid}");
            run_git(&self.root, ["update-ref", &reference, &oid])?;
            return Ok(TemporaryCommit {
                commit: CommitRef {
                    oid,
                    branch: None,
                    temporary: true,
                    parent_oid: Some(parent.oid),
                },
                fingerprint,
            });
        }
        bail!("working files kept changing while super captured them; stop the edits and retry")
    }

    pub fn file_at_commit(&self, oid: &str, path: &str) -> Result<String> {
        run_git(&self.root, ["show", &format!("{oid}:{path}")])
            .with_context(|| format!("could not read `{path}` from commit {}", short_oid(oid)))
    }

    /// Inspect the Git tree directly, without applying source exclusions.
    pub fn path_exists_at_commit(&self, oid: &str, path: &str) -> Result<bool> {
        let output = run_git_bytes(
            &self.root,
            ["--literal-pathspecs", "ls-tree", "--full-tree", "-z", oid, "--", path],
        )?;
        Ok(!output.is_empty())
    }

    pub fn file_bytes_at_commit(&self, oid: &str, path: &str) -> Result<Vec<u8>> {
        run_git_bytes(&self.root, ["show", &format!("{oid}:{path}")])
            .with_context(|| format!("could not read `{path}` from commit {}", short_oid(oid)))
    }

    pub fn source_snapshot(&self, oid: &str, known_hashes: &HashMap<String, String>) -> Result<SourceSnapshot> {
        let mut files = Vec::new();
        let mut source_hashes = known_hashes.clone();
        let mut content_paths = HashMap::new();
        let mut excluded = Vec::new();
        for file in self.source_files_at_commit(oid)? {
            if file.bytes > MAX_SOURCE_FILE_BYTES {
                excluded.push(format!("Excluded `{}` because it is larger than 10 MiB.", file.path));
                continue;
            }
            let sha256 = match source_hashes.get(&file.oid) {
                Some(sha256) if is_lower_sha256(sha256) => sha256.clone(),
                _ => {
                    let content = self.blob_bytes(&file.oid)?;
                    let actual_bytes = u64::try_from(content.len()).unwrap_or(u64::MAX);
                    if actual_bytes != file.bytes {
                        bail!(
                            "Git blob {} changed size while super read commit {}",
                            file.oid,
                            short_oid(oid)
                        )
                    }
                    let sha256 = source_sha256(&content);
                    source_hashes.insert(file.oid.clone(), sha256.clone());
                    sha256
                }
            };
            content_paths.entry(sha256.clone()).or_insert_with(|| file.path.clone());
            files.push(SourceFile {
                path: file.path,
                sha256,
                bytes: file.bytes,
                executable: file.executable,
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let manifest = SourceManifest {
            tree_sha256: source_tree_sha256(&files).map_err(anyhow::Error::msg)?,
            files,
        };
        manifest.validate().map_err(anyhow::Error::msg)?;
        Ok(SourceSnapshot {
            manifest,
            source_hashes,
            excluded,
            content_paths,
        })
    }

    pub fn files_at_commit(&self, oid: &str) -> Result<Vec<String>> {
        Ok(self
            .source_files_at_commit(oid)?
            .into_iter()
            .map(|file| file.path)
            .collect())
    }

    fn source_files_at_commit(&self, oid: &str) -> Result<Vec<TreeFile>> {
        let ignores = self.superignore_at_commit(oid)?;
        regular_tree_files(self.tree_entries_at_commit(oid)?.into_iter().filter(|entry| {
            !ignores
                .matched_path_or_any_parents(self.root.join(&entry.path), false)
                .is_ignore()
        }))
    }

    fn tree_entries_at_commit(&self, oid: &str) -> Result<Vec<TreeEntry>> {
        let output = run_git_bytes(&self.root, ["ls-tree", "-r", "-z", "--long", oid])?;
        let mut entries = Vec::new();
        for entry in output.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
            entries.push(parse_tree_entry(entry)?);
        }
        Ok(entries)
    }

    pub fn file_size_at_commit(&self, oid: &str, path: &str) -> Result<u64> {
        let value = run_git(&self.root, ["cat-file", "-s", &format!("{oid}:{path}")])?;
        value
            .trim()
            .parse()
            .with_context(|| format!("git returned an invalid size for `{path}`"))
    }

    fn status_snapshot(&self) -> Result<Vec<u8>> {
        run_git_bytes(
            &self.root,
            ["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
        )
    }

    fn untracked_paths(&self, include_excluded: bool) -> Result<Vec<String>> {
        let output = run_git_bytes(&self.root, ["ls-files", "--others", "--exclude-standard", "-z"])?;
        let mut builder = GitignoreBuilder::new(&self.root);
        let superignore = self.root.join(".superignore");
        if superignore.exists() {
            builder.add(&superignore);
        }
        let ignores = builder
            .build()
            .context("could not parse the repository's .superignore")?;
        output
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                String::from_utf8(path.to_vec()).context("super does not yet support non-UTF-8 repository paths")
            })
            .filter_ok(|path| {
                include_excluded
                    || !ignores
                        .matched_path_or_any_parents(self.root.join(path), false)
                        .is_ignore()
            })
            .collect()
    }

    fn superignore_at_commit(&self, oid: &str) -> Result<ignore::gitignore::Gitignore> {
        let mut builder = GitignoreBuilder::new(&self.root);
        if let Ok(source) = self.file_at_commit(oid, ".superignore") {
            for line in source.lines() {
                builder.add_line(Some(PathBuf::from(".superignore")), line)?;
            }
        }
        builder
            .build()
            .context("could not parse .superignore at the selected commit")
    }

    fn blob_bytes(&self, oid: &str) -> Result<Vec<u8>> {
        run_git_bytes(&self.root, ["cat-file", "blob", oid]).with_context(|| format!("could not read Git blob {oid}"))
    }

    fn write_worktree_tree(&self, index: &Path, include_excluded: bool) -> Result<String> {
        run_git_with_index(&self.root, index, ["read-tree", "HEAD"])?;
        // Include newly staged paths too. Starting only from HEAD would omit them because Git
        // no longer reports them as untracked. Refresh their contents from disk below.
        let staged = run_git_bytes(&self.root, ["ls-files", "--stage", "-z"])?;
        if !run_git_bytes(&self.root, ["ls-files", "--unmerged", "-z"])?.is_empty() {
            bail!("resolve the Git merge conflicts before preparing source")
        }
        let mut command = git_command(&self.root);
        command
            .env("GIT_INDEX_FILE", index)
            .args(["update-index", "-z", "--index-info"]);
        command_with_stdin(&mut command, &staged)?;
        run_git_with_index(&self.root, index, ["add", "-u", "--", "."])?;
        let untracked = self.untracked_paths(include_excluded)?;
        for paths in untracked.chunks(100) {
            let mut command = git_command(&self.root);
            command.env("GIT_INDEX_FILE", index).arg("add").arg("--");
            command.args(paths);
            command_output(&mut command)?;
        }
        Ok(run_git_with_index(&self.root, index, ["write-tree"])?.trim().to_owned())
    }
}

fn validate_fix_diff_paths(diff: &str) -> Result<()> {
    let mut paths = std::collections::BTreeSet::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("diff --git a/") else {
            continue;
        };
        let (left, right) = rest
            .split_once(" b/")
            .context("the proposed fix has a malformed diff header")?;
        if left.is_empty()
            || left != right
            || left.starts_with('/')
            || left.ends_with('/')
            || left
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
            || left.chars().any(char::is_control)
        {
            bail!("the proposed fix contains an unsafe path or a rename")
        }
        paths.insert(left);
    }
    if paths.is_empty() {
        bail!("the proposed fix contains no unified-diff file header")
    }
    Ok(())
}

fn create_commit_object(root: &Path, tree: &str, parent: &str) -> Result<String> {
    let mut command = git_command(root);
    command
        .args(["commit-tree", tree, "-p", parent])
        .env("GIT_AUTHOR_NAME", "Super CLI")
        .env("GIT_AUTHOR_EMAIL", "super@schematic.tech")
        .env("GIT_COMMITTER_NAME", "Super CLI")
        .env("GIT_COMMITTER_EMAIL", "super@schematic.tech")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("could not run `git commit-tree`")?;
    child
        .stdin
        .take()
        .context("could not open git input")?
        .write_all(b"Temporary Super CLI commit\n")?;
    command_output_from(child.wait_with_output()?).map(|value| value.trim().to_owned())
}

fn run_git<'a>(root: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String> {
    command_output(git_command(root).args(args))
}

fn run_git_at<'a>(path: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String> {
    command_output(
        Command::new("git")
            .env_remove("SUPER_ACCESS_TOKEN")
            .arg("-C")
            .arg(path)
            .args(args),
    )
    .with_context(|| format!("`{}` is not inside a Git worktree", path.display()))
}

fn run_git_bytes<'a>(root: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<Vec<u8>> {
    let output = git_command(root).args(args).output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(git_error(&output))
}

fn run_git_with_index<'a>(root: &Path, index: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String> {
    command_output(git_command(root).env("GIT_INDEX_FILE", index).args(args))
}

fn run_git_with_stdin<'a>(root: &Path, args: impl IntoIterator<Item = &'a str>, input: &[u8]) -> Result<()> {
    let mut command = git_command(root);
    command.args(args);
    command_with_stdin(&mut command, input)
}

fn command_with_stdin(command: &mut Command, input: &[u8]) -> Result<()> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("could not run Git")?;
    child
        .stdin
        .take()
        .context("could not open Git input")?
        .write_all(input)?;
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_error(&output))
    }
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.env_remove("SUPER_ACCESS_TOKEN").arg("-C").arg(root);
    command
}

fn command_output(command: &mut Command) -> Result<String> {
    command_output_from(command.output()?)
}

fn command_output_from(output: Output) -> Result<String> {
    if output.status.success() {
        return String::from_utf8(output.stdout).context("git returned non-UTF-8 output");
    }
    Err(git_error(&output))
}

fn git_error(output: &Output) -> anyhow::Error {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    anyhow::anyhow!(if message.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        message
    })
}

fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_tree_entry(entry: &[u8]) -> Result<TreeEntry> {
    let separator = entry
        .iter()
        .position(|byte| *byte == b'\t')
        .context("Git returned a malformed tree entry without a path")?;
    let (metadata, path_with_tab) = entry.split_at(separator);
    let path = path_with_tab.get(1..).context("Git returned a malformed tree path")?;
    let metadata = std::str::from_utf8(metadata).context("Git returned non-UTF-8 tree metadata")?;
    let mut fields = metadata.split_whitespace();
    let mode = fields.next().context("Git tree entry omitted its mode")?;
    let kind = fields.next().context("Git tree entry omitted its type")?;
    let oid = fields.next().context("Git tree entry omitted its object identity")?;
    let size = fields.next().context("Git tree entry omitted its size")?;
    let bytes = (size != "-")
        .then(|| size.parse().context("Git returned an invalid blob size"))
        .transpose()?;
    Ok(TreeEntry {
        path: String::from_utf8(path.to_vec()).context("super does not yet support non-UTF-8 repository paths")?,
        oid: oid.to_owned(),
        mode: mode.to_owned(),
        kind: kind.to_owned(),
        bytes,
    })
}

fn regular_tree_files(entries: impl Iterator<Item = TreeEntry>) -> Result<Vec<TreeFile>> {
    let (regular, mut unsupported): (Vec<_>, Vec<_>) =
        entries.partition(|entry| entry.kind == "blob" && matches!(entry.mode.as_str(), "100644" | "100755"));
    if !unsupported.is_empty() {
        unsupported.sort_by(|left, right| left.path.cmp(&right.path));
        let paths = unsupported
            .iter()
            .map(|entry| {
                let kind = match (entry.kind.as_str(), entry.mode.as_str()) {
                    ("blob", "120000") => "symlink".to_owned(),
                    ("commit", "160000") => "submodule".to_owned(),
                    _ => format!("type `{}`, mode `{}`", entry.kind, entry.mode),
                };
                format!("  {:?} ({kind})", entry.path)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let suggestions = unsupported
            .iter()
            .filter_map(|entry| literal_ignore_pattern(&entry.path))
            .collect::<Vec<_>>();
        let noun = if unsupported.len() == 1 { "entry" } else { "entries" };
        let mut message = format!(
            "super cannot sync {} unsupported Git {noun}:\n{paths}",
            unsupported.len()
        );
        if !suggestions.is_empty() {
            message.push_str("\n\nTo exclude these entries, append these lines to .superignore.\n");
            message.push_str(&suggestions.join("\n"));
            message.push_str("\n\nCommit .superignore before linking, or include it with --dirty when checking.");
        }
        if suggestions.len() != unsupported.len() {
            message.push_str("\n\nPaths containing control characters have no suggested ignore line. Rename or replace them, or exclude a containing directory.");
        }
        message.push_str("\nAlternatively, replace unsupported entries with regular files needed for checking.");
        bail!(message)
    }
    regular
        .into_iter()
        .map(|entry| {
            Ok(TreeFile {
                path: entry.path,
                oid: entry.oid,
                bytes: entry.bytes.context("Git omitted the size of a regular source blob")?,
                executable: entry.mode == "100755",
            })
        })
        .collect()
}

fn literal_ignore_pattern(path: &str) -> Option<String> {
    // Ignore rules are line-based. Do not turn unusual names into broader wildcards.
    if path.chars().any(char::is_control) {
        return None;
    }
    // Anchor to the repository root so a basename does not exclude unrelated nested files.
    let mut pattern = String::from("/");
    for character in path.chars() {
        if character.is_whitespace() {
            // Preserve trailing whitespace. Non-ASCII literals need a single-alternative
            // group because the ignore matcher's character classes operate on bytes.
            pattern.push(if character.is_ascii() { '[' } else { '{' });
            pattern.push(character);
            pattern.push(if character.is_ascii() { ']' } else { '}' });
        } else {
            if matches!(character, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
                pattern.push('\\');
            }
            pattern.push(character);
        }
    }
    Some(pattern)
}

trait FilterOk<T, E>: Iterator<Item = Result<T, E>> + Sized {
    fn filter_ok(self, predicate: impl FnMut(&T) -> bool) -> impl Iterator<Item = Result<T, E>>;
}

impl<I, T, E> FilterOk<T, E> for I
where
    I: Iterator<Item = Result<T, E>>,
{
    fn filter_ok(mut self, mut predicate: impl FnMut(&T) -> bool) -> impl Iterator<Item = Result<T, E>> {
        std::iter::from_fn(move || {
            loop {
                let item = self.next()?;
                match item {
                    Ok(value) if predicate(&value) => return Some(Ok(value)),
                    Ok(_) => {}
                    Err(error) => return Some(Err(error)),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXT_SOURCE: &str = "header\nbefore\nold\nafter\n";
    const CONTEXT_FIX: &str = "diff --git a/value.txt b/value.txt\n--- a/value.txt\n+++ b/value.txt\n@@ -2,3 +2,3 @@\n before\n-old\n+new\n after\n";

    fn fix_repository() -> (TempDir, GitRepository) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        run_git(root, ["init", "-b", "main"]).unwrap();
        run_git(root, ["config", "user.name", "Super Test"]).unwrap();
        run_git(root, ["config", "user.email", "pup-test@example.com"]).unwrap();
        std::fs::write(root.join("value.txt"), CONTEXT_SOURCE).unwrap();
        std::fs::write(root.join(".superignore"), "excluded.txt\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        run_git(root, ["add", "."]).unwrap();
        run_git(root, ["-c", "commit.gpgsign=false", "commit", "-m", "Base"]).unwrap();
        let repository = GitRepository::discover(root).unwrap();
        (directory, repository)
    }

    #[test]
    fn dirty_fix_matches_the_checked_snapshot_and_preserves_files_index_and_refs() {
        let (_directory, repository) = fix_repository();
        let root = &repository.root;
        std::fs::write(root.join("value.txt"), "staged\n").unwrap();
        std::fs::write(root.join("staged.txt"), "staged new file\n").unwrap();
        run_git(root, ["add", "value.txt", "staged.txt"]).unwrap();
        std::fs::write(root.join("value.txt"), format!("local edit\n{CONTEXT_SOURCE}")).unwrap();
        std::fs::write(root.join("staged.txt"), "working new file\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
        std::fs::write(root.join("excluded.txt"), "excluded\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        let checked = repository.temporary_commit(&HashMap::new()).unwrap();
        let source = repository
            .source_snapshot(&checked.commit.oid, &HashMap::new())
            .unwrap();
        assert!(source.manifest.files.iter().any(|file| file.path == "staged.txt"));
        assert_eq!(
            repository.file_at_commit(&checked.commit.oid, "staged.txt").unwrap(),
            "working new file\n"
        );
        let head = repository.head().unwrap();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        let refs = run_git(root, ["show-ref"]).unwrap();

        let prepared = repository
            .prepare_fix(&source.manifest.tree_sha256, CONTEXT_FIX)
            .unwrap();
        assert!(!prepared.source_changed);
        assert!(prepared.apply_error.is_none(), "{:?}", prepared.apply_error);
        repository.apply_fix_diff(&prepared.before, CONTEXT_FIX).unwrap();
        assert_eq!(
            repository.edits_since(&prepared.before).unwrap(),
            ["value.txt".to_owned()].into()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("value.txt")).unwrap(),
            "local edit\nheader\nbefore\nnew\nafter\n"
        );
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(repository.head().unwrap().oid, head.oid);
        assert_eq!(run_git(root, ["show-ref"]).unwrap(), refs);
        for (path, expected) in [
            ("staged.txt", "working new file\n"),
            ("untracked.txt", "untracked\n"),
            ("excluded.txt", "excluded\n"),
            ("ignored.txt", "ignored\n"),
        ] {
            assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), expected);
        }
    }

    #[test]
    fn failed_hunk_rejects_the_entire_patch() {
        let (_directory, repository) = fix_repository();
        std::fs::write(repository.root.join("other.txt"), "different\n").unwrap();
        let diff = format!(
            "{CONTEXT_FIX}diff --git a/other.txt b/other.txt\n--- a/other.txt\n+++ b/other.txt\n@@ -1 +1 @@\n-old\n+new\n"
        );
        let prepared = repository.prepare_fix(&"0".repeat(64), &diff).unwrap();
        assert!(prepared.apply_error.as_ref().unwrap().contains("other.txt"));
        assert!(repository.apply_fix_diff(&prepared.before, &diff).is_err());
        assert_eq!(
            std::fs::read_to_string(repository.root.join("value.txt")).unwrap(),
            CONTEXT_SOURCE
        );
        assert_eq!(
            std::fs::read_to_string(repository.root.join("other.txt")).unwrap(),
            "different\n"
        );
    }

    #[test]
    fn additions_and_deletions_preserve_other_working_files_and_staging() {
        let (_directory, repository) = fix_repository();
        std::fs::write(repository.root.join("notes.txt"), "existing notes\n").unwrap();
        let diff = "diff --git a/value.txt b/value.txt\ndeleted file mode 100644\n--- a/value.txt\n+++ /dev/null\n@@ -1,4 +0,0 @@\n-header\n-before\n-old\n-after\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+new file\n";
        let prepared = repository.prepare_fix(&"0".repeat(64), diff).unwrap();
        assert!(prepared.apply_error.is_none(), "{:?}", prepared.apply_error);
        repository.apply_fix_diff(&prepared.before, diff).unwrap();
        assert_eq!(
            repository.edits_since(&prepared.before).unwrap(),
            ["value.txt".to_owned(), "new.txt".to_owned()].into()
        );
        assert!(!repository.root.join("value.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repository.root.join("new.txt")).unwrap(),
            "new file\n"
        );
        assert_eq!(
            std::fs::read_to_string(repository.root.join("notes.txt")).unwrap(),
            "existing notes\n"
        );
    }

    #[test]
    fn git_configuration_cannot_relax_patch_context() {
        let (_directory, repository) = fix_repository();
        run_git(&repository.root, ["config", "apply.ignoreWhitespace", "change"]).unwrap();
        std::fs::write(repository.root.join("value.txt"), "header\n  before\nold\nafter\n").unwrap();
        let prepared = repository.prepare_fix(&"0".repeat(64), CONTEXT_FIX).unwrap();
        assert!(prepared.apply_error.is_some());
    }

    #[test]
    fn fix_preview_guards_working_files_staging_and_head() {
        for change in ["working", "staging", "head"] {
            let (_directory, repository) = fix_repository();
            std::fs::write(repository.root.join("other.txt"), "existing edit\n").unwrap();
            let prepared = repository.prepare_fix(&"0".repeat(64), CONTEXT_FIX).unwrap();
            match change {
                "working" => std::fs::write(repository.root.join("other.txt"), "later edit\n").unwrap(),
                "staging" => {
                    run_git(&repository.root, ["add", "other.txt"]).unwrap();
                }
                "head" => {
                    run_git(
                        &repository.root,
                        ["-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "Later"],
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let error = repository.apply_fix_diff(&prepared.before, CONTEXT_FIX).unwrap_err();
            assert!(error.to_string().contains("changed after the fix preview"), "{error:#}");
            assert_eq!(
                std::fs::read_to_string(repository.root.join("value.txt")).unwrap(),
                CONTEXT_SOURCE
            );
        }
    }

    #[test]
    fn scope_compares_edits_even_on_previously_dirty_and_excluded_files() {
        let (_directory, repository) = fix_repository();
        std::fs::write(repository.root.join("value.txt"), "existing edit\n").unwrap();
        std::fs::write(repository.root.join("excluded.txt"), "existing excluded file\n").unwrap();
        let before = repository.worktree_snapshot().unwrap();
        assert!(repository.edits_since(&before).unwrap().is_empty());
        // Restoring HEAD is still a new edit, even though `git status` stops listing the path.
        std::fs::write(repository.root.join("value.txt"), CONTEXT_SOURCE).unwrap();
        std::fs::write(repository.root.join("excluded.txt"), "changed\n").unwrap();
        assert_eq!(
            repository.edits_since(&before).unwrap(),
            ["value.txt".to_owned(), "excluded.txt".to_owned()].into()
        );
        run_git(&repository.root, ["add", "excluded.txt"]).unwrap();
        assert!(
            repository
                .edits_since(&before)
                .unwrap_err()
                .to_string()
                .contains("staging changed")
        );
    }

    #[test]
    fn temporary_commit_preserves_head_and_index_and_is_reusable() {
        let directory = tempfile::tempdir().unwrap();
        run_git(directory.path(), ["init", "-b", "main"]).unwrap();
        run_git(directory.path(), ["config", "user.name", "Super Test"]).unwrap();
        run_git(directory.path(), ["config", "user.email", "pup-test@example.com"]).unwrap();
        std::fs::write(directory.path().join("tracked.txt"), "before\n").unwrap();
        std::fs::write(directory.path().join(".superignore"), "private.txt\n").unwrap();
        run_git(directory.path(), ["add", "."]).unwrap();
        run_git(directory.path(), ["commit", "-m", "Initial"]).unwrap();

        std::fs::write(directory.path().join("tracked.txt"), "after\n").unwrap();
        std::fs::write(directory.path().join("new.txt"), "included\n").unwrap();
        std::fs::write(directory.path().join("private.txt"), "excluded\n").unwrap();
        let head_before = run_git(directory.path(), ["rev-parse", "HEAD"]).unwrap();
        let index_before = run_git(directory.path(), ["write-tree"]).unwrap();
        let repository = GitRepository::discover(directory.path()).unwrap();
        assert!(repository.has_commits().unwrap());
        let first = repository.temporary_commit(&HashMap::new()).unwrap();

        assert_eq!(run_git(directory.path(), ["rev-parse", "HEAD"]).unwrap(), head_before);
        assert_eq!(run_git(directory.path(), ["write-tree"]).unwrap(), index_before);
        assert_eq!(
            repository.file_at_commit(&first.commit.oid, "tracked.txt").unwrap(),
            "after\n"
        );
        assert_eq!(
            repository.file_at_commit(&first.commit.oid, "new.txt").unwrap(),
            "included\n"
        );
        assert!(repository.file_at_commit(&first.commit.oid, "private.txt").is_err());

        let known = HashMap::from([(first.fingerprint.clone(), first.commit.oid.clone())]);
        let second = repository.temporary_commit(&known).unwrap();
        assert_eq!(second.commit.oid, first.commit.oid);
        let reference = format!("refs/super/commits/{}", first.commit.oid);
        assert_eq!(
            run_git(directory.path(), ["rev-parse", &reference]).unwrap().trim(),
            first.commit.oid
        );
    }

    #[test]
    fn empty_repository_has_no_commits() {
        let directory = tempfile::tempdir().unwrap();
        run_git(directory.path(), ["init", "-b", "main"]).unwrap();
        let repository = GitRepository::discover(directory.path()).unwrap();
        assert!(!repository.has_commits().unwrap());
    }

    #[test]
    fn fix_diff_recounts_hunks_and_rejects_changes_after_preparation() {
        let directory = tempfile::tempdir().unwrap();
        run_git(directory.path(), ["init", "-b", "main"]).unwrap();
        run_git(directory.path(), ["config", "user.name", "Super Test"]).unwrap();
        run_git(directory.path(), ["config", "user.email", "pup-test@example.com"]).unwrap();
        std::fs::write(directory.path().join("value.txt"), "old\n").unwrap();
        run_git(directory.path(), ["add", "."]).unwrap();
        run_git(directory.path(), ["commit", "-m", "Fix base"]).unwrap();
        let repository = GitRepository::discover(directory.path()).unwrap();
        let base = repository.head().unwrap();
        let tree_sha256 = repository
            .source_snapshot(&base.oid, &HashMap::new())
            .unwrap()
            .manifest
            .tree_sha256;
        let diff =
            "diff --git a/value.txt b/value.txt\n--- a/value.txt\n+++ b/value.txt\n@@ -1,999 +1,999 @@\n-old\n+new\n";

        let prepared = repository.prepare_fix(&tree_sha256, diff).unwrap();
        assert!(!prepared.source_changed);
        assert!(prepared.apply_error.is_none());
        repository.apply_fix_diff(&prepared.before, diff).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.path().join("value.txt")).unwrap(),
            "new\n"
        );
        assert!(repository.apply_fix_diff(&prepared.before, diff).is_err());
    }

    #[test]
    fn superignore_follows_selected_source_without_reading_retired_filenames() {
        let (_directory, repository) = fix_repository();
        let root = &repository.root;
        std::fs::write(root.join(".pupignore"), "legacy.txt\n").unwrap();
        std::fs::write(root.join(".schignore"), "legacy.txt\n").unwrap();
        std::fs::write(root.join("legacy.txt"), "included\n").unwrap();
        std::fs::write(root.join("excluded.txt"), "excluded by committed rules\n").unwrap();
        run_git(root, ["add", "."]).unwrap();
        run_git(root, ["-c", "commit.gpgsign=false", "commit", "-m", "Source selection"]).unwrap();
        let committed = repository.head().unwrap();

        std::fs::write(root.join(".superignore"), "value.txt\nprivate-new.txt\n").unwrap();
        std::fs::write(root.join("private-new.txt"), "excluded by working rules\n").unwrap();
        let temporary = repository.temporary_commit(&HashMap::new()).unwrap();
        for (oid, excluded, included) in [
            (&committed.oid, "excluded.txt", "value.txt"),
            (&temporary.commit.oid, "value.txt", "excluded.txt"),
        ] {
            let paths = repository.files_at_commit(oid).unwrap();
            let snapshot = repository.source_snapshot(oid, &HashMap::new()).unwrap();
            assert!(!paths.iter().any(|path| path == excluded));
            assert!(paths.iter().any(|path| path == included));
            assert!(paths.iter().any(|path| path == "legacy.txt"));
            assert!(!paths.iter().any(|path| path == "private-new.txt"));
            assert_eq!(
                paths,
                snapshot
                    .manifest
                    .files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            repository
                .file_at_commit(&temporary.commit.oid, "private-new.txt")
                .is_err()
        );
    }

    #[test]
    fn source_snapshot_is_exact_sorted_and_reuses_blob_hashes() {
        let directory = tempfile::tempdir().unwrap();
        run_git(directory.path(), ["init", "-b", "main"]).unwrap();
        run_git(directory.path(), ["config", "user.name", "Super Test"]).unwrap();
        run_git(directory.path(), ["config", "user.email", "pup-test@example.com"]).unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        std::fs::create_dir_all(directory.path().join("private")).unwrap();
        std::fs::write(directory.path().join(".superignore"), "private/\n").unwrap();
        std::fs::write(directory.path().join("src/lib.rs"), b"pub fn value() -> u8 { 7 }\n").unwrap();
        std::fs::write(directory.path().join("private/key.txt"), b"not uploaded\n").unwrap();
        run_git(directory.path(), ["add", "."]).unwrap();
        run_git(directory.path(), ["commit", "-m", "Source snapshot"]).unwrap();

        let repository = GitRepository::discover(directory.path()).unwrap();
        let head = repository.head().unwrap();
        let snapshot = repository.source_snapshot(&head.oid, &HashMap::new()).unwrap();
        let paths = snapshot
            .manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![".superignore", "src/lib.rs"]);
        assert!(snapshot.excluded.is_empty());
        assert_eq!(
            snapshot.manifest.tree_sha256,
            source_tree_sha256(&snapshot.manifest.files).unwrap()
        );

        let source_oid = run_git(directory.path(), ["rev-parse", "HEAD:src/lib.rs"])
            .unwrap()
            .trim()
            .to_owned();
        let source_hash = source_sha256(b"pub fn value() -> u8 { 7 }\n");
        assert_eq!(snapshot.source_hashes.get(&source_oid), Some(&source_hash));
        assert_eq!(snapshot.content_path(&source_hash), Some("src/lib.rs"));

        let cached = repository.source_snapshot(&head.oid, &snapshot.source_hashes).unwrap();
        assert_eq!(cached.manifest, snapshot.manifest);
        assert_eq!(cached.source_hashes, snapshot.source_hashes);
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_git_entries_are_reported_together_and_can_be_ignored() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        run_git(directory.path(), ["init", "-b", "main"]).unwrap();
        run_git(directory.path(), ["config", "user.name", "Super Test"]).unwrap();
        run_git(directory.path(), ["config", "user.email", "pup-test@example.com"]).unwrap();
        std::fs::write(directory.path().join("target.rs"), "fn target() {}\n").unwrap();
        std::fs::write(directory.path().join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(directory.path().join(".superignore"), "/ignored/\n").unwrap();
        std::fs::create_dir(directory.path().join("nested")).unwrap();
        std::fs::create_dir(directory.path().join("ignored")).unwrap();
        symlink("target.rs", directory.path().join("alias.rs")).unwrap();
        symlink("../target.rs", directory.path().join("nested/alias.rs")).unwrap();
        symlink("../target.rs", directory.path().join("ignored/alias.rs")).unwrap();
        run_git(directory.path(), ["add", "."]).unwrap();
        run_git(directory.path(), ["update-index", "--chmod=+x", "run.sh"]).unwrap();
        run_git(directory.path(), ["commit", "-m", "Symlink"]).unwrap();

        let repository = GitRepository::discover(directory.path()).unwrap();
        let module_commit = repository.head().unwrap().oid;
        run_git(
            directory.path(),
            [
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{module_commit},vendor/module"),
            ],
        )
        .unwrap();
        run_git(directory.path(), ["commit", "-m", "Gitlink"]).unwrap();
        let head = repository.head().unwrap();
        let error = repository
            .source_snapshot(&head.oid, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("alias.rs"), "{error}");
        assert!(error.contains("nested/alias.rs"), "{error}");
        assert!(error.contains("vendor/module"), "{error}");
        assert!(
            error.starts_with("super cannot sync 3 unsupported Git entries:"),
            "{error}"
        );
        assert!(error.contains("\"alias.rs\" (symlink)"), "{error}");
        assert!(error.contains("\"vendor/module\" (submodule)"), "{error}");
        assert!(!error.contains("ignored/alias.rs"), "{error}");
        assert_eq!(repository.files_at_commit(&head.oid).unwrap_err().to_string(), error);

        let suggestions = suggested_ignore_lines(&error);
        assert_eq!(suggestions, "/alias.rs\n/nested/alias.rs\n/vendor/module");
        std::fs::write(
            directory.path().join(".superignore"),
            format!("/ignored/\n{suggestions}\n"),
        )
        .unwrap();
        run_git(directory.path(), ["add", ".superignore"]).unwrap();
        run_git(directory.path(), ["commit", "-m", "Ignore symlink"]).unwrap();
        let head = repository.head().unwrap();
        let snapshot = repository.source_snapshot(&head.oid, &HashMap::new()).unwrap();
        let paths = repository.files_at_commit(&head.oid).unwrap();
        assert_eq!(paths, [".superignore", "run.sh", "target.rs"]);
        assert_eq!(
            snapshot
                .manifest
                .files
                .iter()
                .map(|file| &file.path)
                .collect::<Vec<_>>(),
            paths.iter().collect::<Vec<_>>()
        );
        assert!(
            snapshot
                .manifest
                .files
                .iter()
                .find(|file| file.path == "run.sh")
                .unwrap()
                .executable
        );
    }

    #[cfg(unix)]
    fn suggested_ignore_lines(error: &str) -> &str {
        error
            .split_once("To exclude these entries, append these lines to .superignore.\n")
            .unwrap()
            .1
            .split("\n\n")
            .next()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_control_character_paths_are_displayed_without_unsafe_ignore_suggestions() {
        use std::os::unix::fs::symlink;

        let (directory, repository) = fix_repository();
        let root = directory.path();
        symlink("value.txt", root.join("line\nbreak")).unwrap();
        symlink("value.txt", root.join("normal-link")).unwrap();
        run_git(root, ["add", "."]).unwrap();
        run_git(root, ["commit", "-m", "Unusual symlink name"]).unwrap();
        let error = repository
            .source_snapshot(&repository.head().unwrap().oid, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains(r#""line\nbreak""#), "{error}");
        assert!(!error.contains("line\nbreak"), "{error}");
        assert_eq!(suggested_ignore_lines(&error), "/normal-link");
        assert!(error.contains("Paths containing control characters have no suggested ignore line"));
    }

    #[cfg(unix)]
    #[test]
    fn suggested_ignores_match_literal_paths_without_excluding_other_source() {
        use std::os::unix::fs::symlink;

        let (directory, repository) = fix_repository();
        let root = directory.path();
        let links = [
            "#hash.rs",
            "!bang.rs",
            "star*.rs",
            "question?.rs",
            "class[ab].rs",
            "brace{one,two}.rs",
            "back\\slash.rs",
            "trailing  ",
            "spaced name.rs",
            "unicode\u{2003}",
        ];
        std::fs::create_dir(root.join("nested")).unwrap();
        for path in links {
            symlink("value.txt", root.join(path)).unwrap();
        }
        let retained = [
            "star123.rs",
            "question1.rs",
            "classa.rs",
            "braceone.rs",
            "trailing",
            "unicode",
            "nested/#hash.rs",
            "nested/spaced name.rs",
        ];
        for path in retained {
            std::fs::write(root.join(path), "keep me\n").unwrap();
        }
        run_git(root, ["add", "."]).unwrap();
        run_git(root, ["commit", "-m", "Literal symlink paths"]).unwrap();
        let error = repository
            .source_snapshot(&repository.head().unwrap().oid, &HashMap::new())
            .unwrap_err()
            .to_string();
        std::fs::write(
            root.join(".superignore"),
            format!("{}\n", suggested_ignore_lines(&error)),
        )
        .unwrap();
        run_git(root, ["add", ".superignore"]).unwrap();
        run_git(root, ["commit", "-m", "Apply suggested ignores"]).unwrap();
        let head = repository.head().unwrap();
        let snapshot = repository.source_snapshot(&head.oid, &HashMap::new()).unwrap();
        let paths = repository.files_at_commit(&head.oid).unwrap();
        assert_eq!(snapshot.manifest.files.len(), retained.len() + 3);
        for path in retained.into_iter().chain([".gitignore", ".superignore", "value.txt"]) {
            assert!(paths.iter().any(|file| file == path), "missing {path}");
        }
    }
}
