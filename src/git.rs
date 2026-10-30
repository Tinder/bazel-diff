//! Git plumbing shared by `serve` and the one-shot CLI.
//!
//! Everything partial-graph hashing reads from git goes through here: resolving a revision to
//! its commit or tree, listing a tree, snapshotting the working tree as a tree object, checking
//! a revision out, and deciding whether one revision's hashes can be derived from another's
//! (see [`crate::partial`]). The workspace is expected to be the repository root, as the paths
//! `git ls-tree` prints are compared with workspace-relative Bazel paths.

use crate::cancel::{self, Cancellation};
use crate::model::HashFileData;
use crate::partial::{self, Classification, ClassifyInputs, Glob, TreeIndex, MODULE_FILES};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

/// A git binary bound to one workspace.
///
/// Subprocesses belong to `cancellation` when it is set, so `serve` kills them with the query
/// that holds the workspace; the CLI runs them plainly.
#[derive(Clone, Debug)]
pub struct Git {
    path: PathBuf,
    workspace: PathBuf,
    cancellation: Option<Arc<Cancellation>>,
}

impl Git {
    pub fn new(path: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            workspace: workspace.into(),
            cancellation: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: Option<Arc<Cancellation>>) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Runs git in the workspace and returns its output, failing only when it cannot be spawned.
    pub fn output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with_env(args, &[])
    }

    fn output_with_env<I, S>(&self, args: I, env: &[(&str, &OsStr)]) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.path);
        command
            .current_dir(&self.workspace)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null());
        cancel::output(self.cancellation.as_deref(), &mut command)
            .with_context(|| format!("failed to execute {}", self.path.display()))
    }

    /// Runs git, failing on a non-zero exit with the command and its stderr.
    pub fn run<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.stdout(args).map(drop)
    }

    /// Runs git and returns its stdout, failing on a non-zero exit with the command and stderr.
    pub fn stdout<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.stdout_with_env(args, &[])
    }

    fn stdout_with_env<I, S>(&self, args: I, env: &[(&str, &OsStr)]) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect::<Vec<OsString>>();
        let output = self.output_with_env(&args, env)?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.iter()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(output.stdout)
    }

    fn stdout_line<I, S>(&self, args: I, env: &[(&str, &OsStr)]) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(String::from_utf8_lossy(&self.stdout_with_env(args, env)?)
            .trim()
            .to_owned())
    }

    /// Resolves a revision to its commit SHA.
    pub fn resolve_sha(&self, revision: &str) -> Result<String> {
        let output = self.output(["rev-parse", "--verify", &format!("{revision}^{{commit}}")])?;
        if !output.status.success() {
            bail!("revision '{revision}' is missing from the local clone");
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if sha.len() != 40 {
            bail!("revision '{revision}' resolved to invalid SHA '{sha}'");
        }
        Ok(sha)
    }

    /// Resolves the tree object a revision points at; distinct commits with identical content
    /// share it. A tree id resolves to itself.
    pub fn resolve_tree(&self, revision: &str) -> Result<String> {
        let output = self.output(["rev-parse", "--verify", &format!("{revision}^{{tree}}")])?;
        if !output.status.success() {
            bail!("tree for revision '{revision}' is missing from the local clone");
        }
        let tree = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if tree.len() != 40 {
            bail!("revision '{revision}' resolved to invalid tree '{tree}'");
        }
        Ok(tree)
    }

    /// Every tracked path at a revision with its object id and mode, from one `git ls-tree`,
    /// tagged with the revision's tree id.
    pub fn tree_index(&self, revision: &str) -> Result<TreeIndex> {
        let tree = self.resolve_tree(revision)?;
        let output = self.stdout(["ls-tree", "-r", "-z", &tree])?;
        Ok(TreeIndex::parse_ls_tree(&output).with_id(tree))
    }

    /// The content of `path` at a revision.
    pub fn show(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        self.stdout(["show", &format!("{revision}:{path}")])
    }

    /// Writes the working tree as a tree object and lists it: every tracked file as it is on
    /// disk, plus untracked files `.gitignore` does not exclude, with staged and unstaged
    /// deletions applied. A clean working tree yields `HEAD`'s own tree, so an entry generated
    /// from it is keyed exactly as one generated from the commit; a dirty one yields a tree of
    /// its own that `git diff` and `git ls-tree` accept like any other.
    ///
    /// `excluded` names files that are not part of the snapshot even when they lie inside the
    /// workspace, tracked or not: the hash files bazel-diff itself reads and writes, which would
    /// otherwise show up as an added or modified path in every diff.
    ///
    /// The repository's index is staged into a temporary copy (so its stat cache spares the
    /// rehash of unchanged files) and the repository's own index is never touched.
    pub fn snapshot_working_tree(&self, excluded: &[&Path]) -> Result<TreeIndex> {
        let top_level = self.stdout_line(["rev-parse", "--show-toplevel"], &[])?;
        let workspace = match (
            fs::canonicalize(&top_level),
            fs::canonicalize(&self.workspace),
        ) {
            (Ok(root), Ok(workspace)) if root == workspace => workspace,
            _ => bail!(
                "--partialGraph needs the workspace to be the repository root: {} is inside the repository at {top_level}",
                self.workspace.display()
            ),
        };
        let staging = tempfile::tempdir().context("failed to create a temporary index")?;
        let index = staging.path().join("index");
        let repository_index = self.stdout_line(["rev-parse", "--git-path", "index"], &[])?;
        let repository_index = self.workspace.join(repository_index);
        if repository_index.is_file() {
            fs::copy(&repository_index, &index).with_context(|| {
                format!(
                    "failed to copy {} to a temporary index",
                    repository_index.display()
                )
            })?;
        }
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        self.stdout_with_env(["add", "--all"], &env)?;
        let excluded = excluded
            .iter()
            .filter_map(|path| workspace_relative_file(&workspace, path))
            .collect::<Vec<_>>();
        if !excluded.is_empty() {
            let mut args = vec![
                OsString::from("update-index"),
                OsString::from("--force-remove"),
                OsString::from("--"),
            ];
            args.extend(excluded.into_iter().map(OsString::from));
            self.stdout_with_env(&args, &env)?;
        }
        let tree = self.stdout_line(["write-tree"], &env)?;
        if tree.len() != 40 {
            bail!("git write-tree produced invalid tree '{tree}'");
        }
        self.tree_index(&tree)
    }

    /// Checks `sha` out, clearing a stale `index.lock` a killed git left behind once.
    pub fn checkout(&self, sha: &str) -> Result<()> {
        if let Ok(output) = self.output(["rev-parse", "HEAD"]) {
            if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == sha {
                return Ok(());
            }
        }
        let args = [
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--force",
            sha,
        ];
        if self.run(args).is_ok() {
            return Ok(());
        }
        let index_lock = self.workspace.join(".git/index.lock");
        if !index_lock.is_file() {
            bail!("git checkout failed for revision {sha}");
        }
        fs::remove_file(&index_lock).context("failed to remove stale .git/index.lock")?;
        eprintln!(
            "[Warn] cleared stale git index.lock in {}; retrying checkout of {sha}",
            self.workspace.display()
        );
        self.run(args)
    }
}

/// `path` relative to the canonical `workspace` when it lies inside it, with the file itself
/// allowed not to exist yet (an output about to be written): its parent is canonicalized and
/// the file name appended.
fn workspace_relative_file(workspace: &Path, path: &Path) -> Option<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let name = absolute.file_name()?;
    let parent = fs::canonicalize(absolute.parent()?).ok()?;
    let relative = parent
        .join(name)
        .strip_prefix(workspace)
        .ok()?
        .to_path_buf();
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// The configuration a derivation decision depends on besides the two revisions.
#[derive(Clone, Copy, Debug)]
pub struct DerivationPolicy<'a> {
    pub seed_filepaths: &'a BTreeSet<PathBuf>,
    pub rebuild_patterns: &'a [Glob],
}

/// Decides whether `revision` can be derived from the entry generated at `base_revision`: the
/// raw diff between the two, the module and workspace files' label references at `revision`,
/// and the policy's rebuild patterns and seed files go through [`partial::classify`]. Both
/// arguments may be commits or tree ids.
pub fn classify_revisions(
    git: &Git,
    base_revision: &str,
    revision: &str,
    base: &HashFileData,
    tree: &TreeIndex,
    policy: &DerivationPolicy<'_>,
) -> Result<Classification> {
    let diff = git.stdout([
        "diff",
        "--raw",
        "--no-renames",
        "-z",
        base_revision,
        revision,
    ])?;
    let diff = partial::parse_diff_raw(&diff)?;
    let mut module_referenced = BTreeSet::new();
    for file in MODULE_FILES {
        if tree.get(file).is_none() {
            continue;
        }
        let text = git.show(revision, file)?;
        module_referenced.extend(partial::referenced_main_repo_paths(
            &String::from_utf8_lossy(&text),
        ));
    }
    Ok(partial::classify(&ClassifyInputs {
        diff: &diff,
        tree,
        base,
        workspace: git.workspace(),
        seed_filepaths: policy.seed_filepaths,
        rebuild_patterns: policy.rebuild_patterns,
        module_referenced: &module_referenced,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partial::TreeKind;

    fn git_command(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repository() -> (tempfile::TempDir, Git) {
        let repo = tempfile::tempdir().unwrap();
        git_command(repo.path(), &["init", "-q"]);
        git_command(repo.path(), &["config", "user.email", "test@example.com"]);
        git_command(repo.path(), &["config", "user.name", "Test"]);
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored/\n").unwrap();
        git_command(repo.path(), &["add", "-A"]);
        git_command(repo.path(), &["commit", "-q", "-m", "one"]);
        let git = Git::new("git", repo.path());
        (repo, git)
    }

    #[test]
    fn resolves_commits_and_trees() {
        let (repo, git) = repository();
        let head = git_command(repo.path(), &["rev-parse", "HEAD"]);
        let tree = git_command(repo.path(), &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(git.resolve_sha("HEAD").unwrap(), head);
        assert_eq!(git.resolve_tree("HEAD").unwrap(), tree);
        assert_eq!(git.resolve_tree(&tree).unwrap(), tree);
        assert_eq!(
            git.resolve_sha("missing").unwrap_err().to_string(),
            "revision 'missing' is missing from the local clone"
        );
        assert!(git
            .resolve_tree("missing")
            .unwrap_err()
            .to_string()
            .contains("missing"));
        assert!(git.resolve_sha(&tree).is_err(), "a tree is not a commit");

        let index = git.tree_index("HEAD").unwrap();
        assert_eq!(index.id(), Some(tree.as_str()));
        assert_eq!(
            index.content_hashes()["file.txt"],
            git_command(repo.path(), &["rev-parse", "HEAD:file.txt"])
        );
        assert_eq!(git.show("HEAD", "file.txt").unwrap(), b"one");
        assert!(git.show("HEAD", "nope.txt").is_err());
        assert!(git.tree_index("missing").is_err());
        assert!(git
            .run(["not-a-command"])
            .unwrap_err()
            .to_string()
            .contains("git not-a-command failed"));

        let missing_binary = Git::new("/definitely/missing/git", repo.path());
        assert!(missing_binary.output::<[&str; 0], &str>([]).is_err());
    }

    #[test]
    fn working_tree_snapshot_matches_head_when_clean_and_captures_edits() {
        let (repo, git) = repository();
        let head_tree = git_command(repo.path(), &["rev-parse", "HEAD^{tree}"]);
        let clean = git.snapshot_working_tree(&[]).unwrap();
        assert_eq!(clean.id(), Some(head_tree.as_str()));

        // A modified tracked file, an untracked file and an ignored one, with nothing staged.
        fs::write(repo.path().join("file.txt"), "two").unwrap();
        fs::create_dir_all(repo.path().join("new dir")).unwrap();
        fs::write(repo.path().join("new dir/added.txt"), "added").unwrap();
        fs::create_dir_all(repo.path().join("ignored")).unwrap();
        fs::write(repo.path().join("ignored/skip.txt"), "skip").unwrap();
        let dirty = git.snapshot_working_tree(&[]).unwrap();
        assert_ne!(dirty.id(), clean.id());
        let hashes = dirty.content_hashes();
        assert_eq!(
            hashes["file.txt"],
            git_command(repo.path(), &["hash-object", "file.txt"])
        );
        assert_eq!(
            hashes["new dir/added.txt"],
            git_command(repo.path(), &["hash-object", "new dir/added.txt"])
        );
        assert!(!hashes.contains_key("ignored/skip.txt"));
        assert_eq!(
            dirty.get("file.txt").unwrap().kind,
            TreeKind::File { executable: false }
        );
        // The repository's own index and HEAD are untouched.
        assert_eq!(
            git_command(repo.path(), &["rev-parse", "HEAD^{tree}"]),
            head_tree
        );
        assert_eq!(
            git_command(repo.path(), &["diff", "--cached", "--name-only"]),
            ""
        );
        assert_eq!(
            git_command(repo.path(), &["status", "--porcelain"])
                .lines()
                .count(),
            2
        );
        // The snapshot is a real tree object: the same edits committed yield the same id.
        git_command(repo.path(), &["add", "-A"]);
        git_command(repo.path(), &["commit", "-q", "-m", "two"]);
        assert_eq!(
            git_command(repo.path(), &["rev-parse", "HEAD^{tree}"]),
            dirty.id().unwrap()
        );

        // A deletion is applied even when unstaged.
        fs::remove_file(repo.path().join("file.txt")).unwrap();
        assert!(git
            .snapshot_working_tree(&[])
            .unwrap()
            .get("file.txt")
            .is_none());
    }

    #[test]
    fn working_tree_snapshot_leaves_excluded_files_out() {
        let (repo, git) = repository();
        let head_tree = git_command(repo.path(), &["rev-parse", "HEAD^{tree}"]);
        // An untracked output inside the workspace, one that does not exist yet, a path outside
        // the workspace, and a relative path (against the process working directory).
        let output = repo.path().join("out/hashes.json");
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, "{}").unwrap();
        let pending = repo.path().join("pending.json");
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("hashes.json");
        let snapshot = git
            .snapshot_working_tree(&[&output, &pending, &elsewhere, Path::new("relative.json")])
            .unwrap();
        assert_eq!(
            snapshot.id(),
            Some(head_tree.as_str()),
            "{:?}",
            snapshot.content_hashes()
        );
        // A tracked file is left out too, and the repository's index is untouched.
        let tracked = repo.path().join("file.txt");
        let without_tracked = git.snapshot_working_tree(&[&tracked]).unwrap();
        assert!(without_tracked.get("file.txt").is_none());
        assert_ne!(without_tracked.id(), snapshot.id());
        assert_eq!(
            git_command(repo.path(), &["status", "--porcelain", "file.txt"]),
            ""
        );
        let workspace = fs::canonicalize(repo.path()).unwrap();
        assert_eq!(workspace_relative_file(&workspace, outside.path()), None);
        assert_eq!(
            workspace_relative_file(&workspace, &repo.path().join("missing dir/x.json")),
            None
        );
        assert_eq!(
            workspace_relative_file(&workspace, &output).as_deref(),
            Some("out/hashes.json")
        );
    }

    #[test]
    fn working_tree_snapshot_requires_the_repository_root() {
        let (repo, _) = repository();
        let nested = repo.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        let error = Git::new("git", &nested)
            .snapshot_working_tree(&[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("repository root"), "{error}");
        let outside = tempfile::tempdir().unwrap();
        assert!(Git::new("git", outside.path())
            .snapshot_working_tree(&[])
            .is_err());
    }

    #[test]
    fn working_tree_snapshot_without_an_index_file() {
        let repo = tempfile::tempdir().unwrap();
        git_command(repo.path(), &["init", "-q"]);
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        assert!(!repo.path().join(".git/index").exists());
        let snapshot = Git::new("git", repo.path())
            .snapshot_working_tree(&[])
            .unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.get("file.txt").is_some());
    }

    #[test]
    fn checkout_switches_revisions_and_clears_a_stale_index_lock() {
        let (repo, git) = repository();
        let first = git_command(repo.path(), &["rev-parse", "HEAD"]);
        fs::write(repo.path().join("file.txt"), "two").unwrap();
        git_command(repo.path(), &["commit", "-q", "-am", "two"]);
        let second = git_command(repo.path(), &["rev-parse", "HEAD"]);
        git.checkout(&first).unwrap();
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "one"
        );
        // Already there: a no-op even with a stale lock in place.
        fs::write(repo.path().join(".git/index.lock"), "").unwrap();
        git.checkout(&first).unwrap();
        git.checkout(&second).unwrap();
        assert!(!repo.path().join(".git/index.lock").exists());
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "two"
        );
        assert!(git.checkout("missing").is_err());
    }

    #[test]
    fn classifies_source_edits_against_a_base_entry() {
        let (repo, git) = repository();
        let base_tree = git.resolve_tree("HEAD").unwrap();
        let base = HashFileData {
            partial: Some(partial::PartialRecipes {
                version: partial::RECIPE_VERSION,
                seed_hash: "00".repeat(32),
                package_seeds: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let seeds = BTreeSet::new();
        let policy = DerivationPolicy {
            seed_filepaths: &seeds,
            rebuild_patterns: &[],
        };
        fs::write(repo.path().join("file.txt"), "two").unwrap();
        let edited = git.snapshot_working_tree(&[]).unwrap();
        let derivable = classify_revisions(
            &git,
            &base_tree,
            edited.id().unwrap(),
            &base,
            &edited,
            &policy,
        )
        .unwrap();
        assert!(
            matches!(&derivable, Classification::Derivable { unmatched_paths, .. } if unmatched_paths == &["file.txt"]),
            "{derivable:?}"
        );
        fs::write(repo.path().join("BUILD"), "").unwrap();
        let rebuilt = git.snapshot_working_tree(&[]).unwrap();
        let rebuild = classify_revisions(
            &git,
            &base_tree,
            rebuilt.id().unwrap(),
            &base,
            &rebuilt,
            &policy,
        )
        .unwrap();
        assert!(
            matches!(rebuild, Classification::Rebuild { .. }),
            "{rebuild:?}"
        );
        // A module file at the revision contributes its label references.
        fs::write(
            repo.path().join("MODULE.bazel"),
            "bazel_dep(name = \"x\")\nuse_repo_rule(\"//:deps.bzl\")\n",
        )
        .unwrap();
        let with_module = git.snapshot_working_tree(&[]).unwrap();
        assert!(classify_revisions(
            &git,
            &base_tree,
            with_module.id().unwrap(),
            &base,
            &with_module,
            &policy
        )
        .is_ok());
        assert!(classify_revisions(&git, "missing", &base_tree, &base, &edited, &policy).is_err());
    }
}
