//! Opt-in partial graph updates for `bazel-diff serve` (`--partialGraph`).
//!
//! A full hash generation pays for a `bazel query` over the whole graph even when the change
//! between two revisions cannot have altered that graph: editing the body of a source file
//! changes what Bazel builds, never which targets exist or how they depend on each other. In
//! partial-graph mode every generated cache entry carries a *recipe* for each target -- the
//! digest of everything that goes into its hash except the content of source files, plus the
//! ordered source inputs and rule dependencies it reads -- and a revision whose diff from a
//! cached base touches only regular source content is hashed by re-evaluating the base's recipes
//! against the new revision's `git ls-tree`, with no Bazel invocation at all.
//!
//! The one property everything rests on: **a full generation in this mode and a derivation from
//! a base entry produce identical entries**, because both compute every hash from the recipe
//! through [`evaluate`]. The full generation records the recipe while walking the query result,
//! then evaluates it; the derivation reuses the recorded recipe. The nightly
//! `tools/perf/partial_graph_bench.py` harness checks this equality on every scenario it runs.
//!
//! Hashes in this mode differ from the default mode's (the rule hash folds in source digests
//! after a digest of the rest, rather than interleaved), which is why the flag is part of the
//! serve cache fingerprint: entries of the two modes are never compared with one another.
//!
//! [`classify`] decides when a derivation is allowed. It is deliberately conservative: anything
//! other than a content change to a regular file (an added, deleted or renamed path, a mode
//! change to or from a symlink or submodule), a change to a file Bazel reads while *loading*
//! the graph (`BUILD`, `.bzl`, `MODULE.bazel`, `WORKSPACE`, `.bazelrc`, ...), a file a module
//! or workspace file names by label (lock files a module extension consumes), a seed file, or a
//! path matching a user-supplied `--partialGraphRebuildPattern` falls back to a full generation.
//! A fallback costs speed, never correctness.

use crate::bazel::{attribute_main_repo_labels, target_label};
use crate::hash::{hash_pool, is_main_repo, label_package, main_repo_path};
use crate::model::{HashFileData, TargetHash};
use crate::proto::blaze_query::Rule;
use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

type DigestBytes = [u8; 32];

/// Bumped when the recipe format or the evaluation formula changes, so an entry an older
/// release wrote is regenerated rather than evaluated under a formula it was not recorded for.
pub const RECIPE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Git tree index
// ---------------------------------------------------------------------------------------------

/// What a tracked path is at a revision, from its `git ls-tree` mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeKind {
    File { executable: bool },
    Symlink,
    Submodule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    /// The object id: a blob for files and symlinks, a commit for submodules.
    pub object: String,
    pub kind: TreeKind,
}

/// Every tracked path of one revision, from `git ls-tree -r -z <rev>`.
///
/// This is the only view of source content partial-graph hashing ever reads: file content is
/// the blob id, the executable bit is the mode, a directory is the set of entries under it. The
/// working tree is never consulted, so a derivation needs no checkout and a full generation
/// hashes exactly what a derivation would.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeIndex {
    entries: BTreeMap<String, TreeEntry>,
}

impl TreeIndex {
    /// Parses `git ls-tree -r -z` records: `<mode> <type> <object>\t<path>\0`. Tree entries
    /// (only printed with `-t`) and unknown modes are skipped.
    pub fn parse_ls_tree(output: &[u8]) -> Self {
        let entries = output
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .filter_map(|record| {
                let tab = record.iter().position(|byte| *byte == b'\t')?;
                let metadata = String::from_utf8_lossy(&record[..tab]);
                let mut fields = metadata.split(' ');
                let mode = fields.next()?;
                let object_type = fields.next()?;
                let object = fields.next()?.to_owned();
                let kind = match (mode, object_type) {
                    ("100644", "blob") | ("100664", "blob") => TreeKind::File { executable: false },
                    ("100755", "blob") => TreeKind::File { executable: true },
                    ("120000", "blob") => TreeKind::Symlink,
                    ("160000", "commit") => TreeKind::Submodule,
                    _ => return None,
                };
                let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
                Some((path, TreeEntry { object, kind }))
            })
            .collect();
        Self { entries }
    }

    pub fn get(&self, path: &str) -> Option<&TreeEntry> {
        self.entries.get(path)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries below `directory`, in path order, keyed by their path relative to it.
    pub fn under<'a>(&'a self, directory: &str) -> impl Iterator<Item = (&'a str, &'a TreeEntry)> {
        let prefix = if directory.is_empty() || directory.ends_with('/') {
            directory.to_owned()
        } else {
            format!("{directory}/")
        };
        let end = format!("{prefix}\u{10FFFF}");
        let strip = prefix.len();
        self.entries
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Excluded(end.as_str()),
            ))
            .map(move |(path, entry)| (&path[strip..], entry))
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// The `{path: blob}` map `--contentHashPath` accepts, as the serve full-content path has
    /// always seeded it: every blob (files and symlinks), no submodules.
    pub fn content_hashes(&self) -> BTreeMap<String, String> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.kind != TreeKind::Submodule)
            .map(|(path, entry)| (path.clone(), entry.object.clone()))
            .collect()
    }

    /// Paths of the symlinks in the tree.
    pub fn symlinks(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.kind == TreeKind::Symlink)
            .map(|(path, _)| path.as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Source digests from the tree
// ---------------------------------------------------------------------------------------------

/// The digest of a source label at a revision, from the tree alone.
///
/// Mirrors the shape of the default mode's source digest (content, a kind tag, the seed, the
/// label) with git's view standing in for the working tree: a file is its blob id and mode, a
/// symlink its link blob, a submodule its commit, a directory the ordered digest of everything
/// under it, and a label outside the main repository the digest of nothing, as in the default
/// mode without fine-grained external repositories.
pub fn source_digest(tree: &TreeIndex, label: &str, seed: &[u8]) -> DigestBytes {
    let Some(path) = main_repo_path(label) else {
        return Sha256::digest([]).into();
    };
    let path = path.to_string_lossy().into_owned();
    let mut hasher = Sha256::new();
    match tree.get(&path) {
        Some(entry) => update_entry(&mut hasher, entry),
        None => {
            let mut directory = Sha256::new();
            let mut populated = false;
            for (relative, entry) in tree.under(&path) {
                populated = true;
                directory.update(relative.as_bytes());
                directory.update([0]);
                update_entry(&mut directory, entry);
                directory.update([0]);
            }
            if populated {
                hasher.update(directory.finalize());
                hasher.update([2]);
            } else {
                hasher.update([0]);
            }
        }
    }
    hasher.update(seed);
    hasher.update(label.as_bytes());
    hasher.finalize().into()
}

fn update_entry(hasher: &mut Sha256, entry: &TreeEntry) {
    hasher.update(entry.object.as_bytes());
    match entry.kind {
        TreeKind::File { executable } => {
            hasher.update([1]);
            hasher.update([u8::from(executable)]);
        }
        TreeKind::Symlink => hasher.update([3]),
        TreeKind::Submodule => hasher.update([4]),
    }
}

/// The digest of a main-repository file that is not a target (a `.bzl` a package loads, a rule
/// input the query did not list), or `None` when the label does not name a tracked file.
pub fn soft_digest(tree: &TreeIndex, label: &str) -> Option<DigestBytes> {
    if !is_main_repo(label) {
        return None;
    }
    let path = main_repo_path(label)?;
    let entry = tree.get(&path.to_string_lossy())?;
    if entry.kind == TreeKind::Submodule {
        return None;
    }
    Some(source_digest(tree, label, &[]))
}

// ---------------------------------------------------------------------------------------------
// Recipes
// ---------------------------------------------------------------------------------------------

/// One rule's hashing recipe, stored as `[static, sources, deps]`.
///
/// `static` is the digest of every direct contribution that is not a source digest, in the
/// order the default mode feeds them: the rule definition, the seed-file hash, the `.bzl` seed,
/// each input's label, the visibility package groups. `sources` are the labels whose source
/// digest follows, in input order; `deps` the rule keys whose overall digest the transitive hash
/// folds in, in input order.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuleRecipe(pub String, pub Vec<String>, pub Vec<String>);

impl RuleRecipe {
    pub fn new(static_digest: DigestBytes, sources: Vec<String>, deps: Vec<String>) -> Self {
        Self(hex::encode(static_digest), sources, deps)
    }

    pub fn static_digest(&self) -> Result<DigestBytes> {
        decode_digest(&self.0)
    }

    pub fn sources(&self) -> &[String] {
        &self.1
    }

    pub fn deps(&self) -> &[String] {
        &self.2
    }
}

fn decode_digest(value: &str) -> Result<DigestBytes> {
    let bytes = hex::decode(value).context("recipe digest is not hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("recipe digest is not 32 bytes"))
}

/// Everything a cache entry needs to be re-hashed against another revision's tree.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialRecipes {
    pub version: u32,
    /// Hex digest of the `--seed-filepaths` contents.
    pub seed_hash: String,
    /// Whether packages seed their source files with the `.bzl` files they load (the default
    /// mode does so only when the query carries no instantiation stacks).
    pub package_seeds: bool,
    /// The `.bzl` labels each package's source files load, in query order, for packages where
    /// the list is non-empty. A source file whose list differs from its package's is listed in
    /// `source_subincludes` instead.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_subincludes: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_subincludes: BTreeMap<String, Vec<String>>,
    pub rules: BTreeMap<String, RuleRecipe>,
    /// Generated file label to the key of the rule that generates it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub generated: BTreeMap<String, String>,
    /// Workspace-relative files and directories read while the graph loads whose content is
    /// folded into a recipe's static digest rather than tracked as a source target: local
    /// repository roots, the files repository rules name by label, and the files Bazel's
    /// repository marker files list. A change under any of them forces a full generation.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub load_inputs: BTreeSet<String>,
}

impl PartialRecipes {
    fn subincludes(&self, label: &str) -> &[String] {
        self.source_subincludes
            .get(label)
            .or_else(|| self.package_subincludes.get(label_package(label)))
            .map_or(&[], Vec::as_slice)
    }
}

/// Collects recipes while the default traversal hashes a query result.
pub(crate) struct RecipeRecorder {
    recipes: PartialRecipes,
    /// Every source's subinclude list, grouped by package and then by list, until `finish`
    /// picks each package's default: `--order_output=no` makes the query order arbitrary, so
    /// the default must not depend on which source was seen first.
    subincludes: BTreeMap<String, BTreeMap<Vec<String>, Vec<String>>>,
}

impl RecipeRecorder {
    pub(crate) fn new(seed_hash: DigestBytes, package_seeds: bool) -> Self {
        Self {
            recipes: PartialRecipes {
                version: RECIPE_VERSION,
                seed_hash: hex::encode(seed_hash),
                package_seeds,
                ..Default::default()
            },
            subincludes: BTreeMap::new(),
        }
    }

    pub(crate) fn record_source(&mut self, label: &str, subincludes: &[String]) {
        self.subincludes
            .entry(label_package(label).to_owned())
            .or_default()
            .entry(subincludes.to_vec())
            .or_default()
            .push(label.to_owned());
    }

    pub(crate) fn record_loading_inputs(&mut self, paths: impl IntoIterator<Item = String>) {
        self.recipes.load_inputs.extend(paths);
    }

    pub(crate) fn record_generated(&mut self, label: &str, generating_rule: &str) {
        self.recipes
            .generated
            .insert(label.to_owned(), generating_rule.to_owned());
    }

    pub(crate) fn record_rule(&mut self, key: &str, recipe: RuleRecipe) {
        self.recipes.rules.insert(key.to_owned(), recipe);
    }

    pub(crate) fn finish(mut self) -> PartialRecipes {
        for (package, lists) in std::mem::take(&mut self.subincludes) {
            // The package default is the list of its lexicographically first source, so the
            // serialized recipes do not depend on query order; every other list is an override.
            let default = lists
                .iter()
                .min_by_key(|(_, labels)| labels.iter().min().cloned())
                .map(|(list, _)| list.clone())
                .unwrap_or_default();
            for (list, labels) in lists {
                if list == default {
                    continue;
                }
                for label in labels {
                    self.recipes.source_subincludes.insert(label, list.clone());
                }
            }
            if !default.is_empty() {
                self.recipes.package_subincludes.insert(package, default);
            }
        }
        self.recipes
    }
}

/// The workspace paths a repository rule's definition digest folds in: the root of a
/// `local_repository` (its whole tree is hashed) and every main-repository label among its
/// attributes (a `build_file`, `patches`, a requirements file). Paths outside the workspace are
/// not in git and are left out.
pub(crate) fn rule_loading_inputs(rule: &Rule, workspace: &Path) -> Vec<String> {
    if !rule.name.starts_with("//external:") {
        return Vec::new();
    }
    let mut inputs = Vec::new();
    if rule.rule_class == "local_repository" {
        if let Some(path) = rule
            .attribute
            .iter()
            .find(|attribute| attribute.name == "path")
            .and_then(|attribute| attribute.string_value.as_deref())
        {
            inputs.extend(normalize_workspace_path(workspace, path));
        }
    }
    inputs.extend(
        attribute_main_repo_labels(&rule.attribute)
            .filter_map(label_path)
            .filter(|path| !path.is_empty()),
    );
    inputs
}

/// The main-repository files Bazel's repository marker files (`<output base>/external/*.marker`)
/// record as read by a fetched repository rule: `FILE:@@//path <hash>` lines. Nothing when the
/// directory cannot be read.
pub(crate) fn marker_loading_inputs(output_base: &Path) -> BTreeSet<String> {
    let mut inputs = BTreeSet::new();
    let Ok(entries) = fs::read_dir(output_base.join("external")) else {
        return inputs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "marker")
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        inputs.extend(marker_file_inputs(&text));
    }
    inputs
}

/// The main-repository paths one marker file's `FILE:` lines name.
pub fn marker_file_inputs(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines()
        .filter_map(|line| line.strip_prefix("FILE:"))
        .filter_map(|rest| rest.split(' ').next())
        .filter(|label| is_main_repo(label))
        .filter_map(label_path)
        .filter(|path| !path.is_empty())
}

/// A workspace path as git names it: `%workspace%/x`, `./x`, `a/../x` and an absolute path
/// inside the workspace all become `x`. `None` for a path that leaves the workspace.
pub fn normalize_workspace_path(workspace: &Path, value: &str) -> Option<String> {
    let value = value
        .strip_prefix("%workspace%/")
        .or_else(|| value.strip_prefix("%workspace%"))
        .unwrap_or(value);
    let path = Path::new(value);
    let relative = if path.is_absolute() {
        match path.strip_prefix(workspace) {
            Ok(stripped) => stripped.to_path_buf(),
            Err(_) => {
                let canonical = fs::canonicalize(workspace).ok()?;
                path.strip_prefix(&canonical).ok()?.to_path_buf()
            }
        }
    } else {
        path.to_path_buf()
    };
    let mut parts: Vec<String> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

// ---------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------

/// Computes every target's hash from the recipes and the revision's tree.
///
/// `targets` lists every label with its kind (`Rule`, `GeneratedFile`, `SourceFile`); it is
/// visited in label order, which fixes which member of a dependency cycle is entered first, as
/// the default traversal does.
pub fn evaluate(
    recipes: &PartialRecipes,
    targets: &BTreeMap<String, String>,
    tree: &TreeIndex,
) -> Result<BTreeMap<String, TargetHash>> {
    if recipes.version != RECIPE_VERSION {
        bail!(
            "recipes were recorded with format version {}, this release evaluates version {RECIPE_VERSION}",
            recipes.version
        );
    }
    let seed_hash = decode_digest(&recipes.seed_hash)?;
    let source_labels = targets
        .iter()
        .filter(|(_, kind)| kind.as_str() == "SourceFile")
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>();
    let source_digests = hash_pool().install(|| {
        source_labels
            .par_iter()
            .map(|label| {
                let mut seed = Sha256::new();
                seed.update(label.as_bytes());
                for subinclude in recipes.subincludes(label) {
                    seed.update(subinclude.as_bytes());
                }
                let seed: DigestBytes = seed.finalize().into();
                ((*label).to_owned(), source_digest(tree, label, &seed))
            })
            .collect::<HashMap<_, _>>()
    });
    let package_seeds = if recipes.package_seeds {
        package_seeds(recipes, &source_labels, tree)
    } else {
        BTreeMap::new()
    };

    let mut evaluator = Evaluator {
        recipes,
        tree,
        source_digests,
        soft_digests: HashMap::new(),
        cache: HashMap::new(),
    };
    let mut hashes = BTreeMap::new();
    let mut path = Vec::new();
    for (label, kind) in targets {
        let (overall, direct) = match kind.as_str() {
            "Rule" => evaluator.rule(label, &mut path)?,
            "GeneratedFile" => {
                let generating_rule = recipes
                    .generated
                    .get(label)
                    .ok_or_else(|| anyhow!("recipes do not name the rule generating {label}"))?;
                evaluator.rule(generating_rule, &mut path)?
            }
            "SourceFile" => {
                let mut hasher = Sha256::new();
                hasher.update(evaluator.source_digests[label.as_str()]);
                hasher.update(seed_hash);
                if let Some(seed) = package_seeds.get(label_package(label)) {
                    hasher.update(seed);
                }
                let digest: DigestBytes = hasher.finalize().into();
                (digest, digest)
            }
            other => bail!("cannot evaluate a target of kind {other} ({label})"),
        };
        hashes.insert(
            label.clone(),
            TargetHash {
                kind: kind.clone(),
                hash: overall.into(),
                direct_hash: direct.into(),
            },
        );
    }
    Ok(hashes)
}

/// Per-package digest of the `.bzl` files the package's source files load, mirroring the
/// default mode's `package_bzl_seeds`: the union over the package's sources, sorted, counting
/// only labels that name a tracked file.
fn package_seeds(
    recipes: &PartialRecipes,
    source_labels: &[&str],
    tree: &TreeIndex,
) -> BTreeMap<String, DigestBytes> {
    let mut packages = BTreeMap::<&str, BTreeSet<&str>>::new();
    for label in source_labels {
        let subincludes = recipes.subincludes(label);
        if subincludes.is_empty() {
            continue;
        }
        packages
            .entry(label_package(label))
            .or_default()
            .extend(subincludes.iter().map(String::as_str));
    }
    let mut digests = HashMap::new();
    let mut seeds = BTreeMap::new();
    for (package, labels) in packages {
        let mut hasher = Sha256::new();
        let mut tracked = false;
        for label in labels {
            let digest = *digests
                .entry(label)
                .or_insert_with(|| soft_digest(tree, label));
            if let Some(digest) = digest {
                tracked = true;
                hasher.update(label.as_bytes());
                hasher.update(digest);
            }
        }
        if tracked {
            seeds.insert(package.to_owned(), hasher.finalize().into());
        }
    }
    seeds
}

struct Evaluator<'a> {
    recipes: &'a PartialRecipes,
    tree: &'a TreeIndex,
    source_digests: HashMap<String, DigestBytes>,
    soft_digests: HashMap<String, Option<DigestBytes>>,
    cache: HashMap<String, (DigestBytes, DigestBytes)>,
}

impl Evaluator<'_> {
    fn rule(&mut self, key: &str, path: &mut Vec<String>) -> Result<(DigestBytes, DigestBytes)> {
        if let Some(digest) = self.cache.get(key) {
            return Ok(*digest);
        }
        let recipe = self
            .recipes
            .rules
            .get(key)
            .ok_or_else(|| anyhow!("recipes hold no rule {key}"))?;
        let static_digest = recipe.static_digest()?;
        if path.iter().any(|entry| entry == key) {
            // A back edge: the rule stands in with a digest of its own definition, uncached,
            // exactly as the default traversal does.
            let mut hasher = Sha256::new();
            hasher.update(b"cycle");
            hasher.update(static_digest);
            let placeholder: DigestBytes = hasher.finalize().into();
            return Ok((placeholder, placeholder));
        }
        path.push(key.to_owned());
        let mut direct = Sha256::new();
        direct.update(static_digest);
        for source in recipe.sources() {
            let digest = match self.source_digests.get(source) {
                Some(digest) => *digest,
                None => {
                    let soft = *self
                        .soft_digests
                        .entry(source.clone())
                        .or_insert_with(|| soft_digest(self.tree, source));
                    match soft {
                        Some(digest) => digest,
                        None => bail!(
                            "source input {source} of {key} is neither a target nor a tracked file"
                        ),
                    }
                }
            };
            direct.update(digest);
        }
        let direct: DigestBytes = direct.finalize().into();
        let mut overall = Sha256::new();
        for dep in recipe.deps() {
            let (dep_overall, _) = self.rule(dep, path)?;
            overall.update(dep_overall);
        }
        overall.update(direct);
        let digest = (overall.finalize().into(), direct);
        path.pop();
        self.cache.insert(key.to_owned(), digest);
        Ok(digest)
    }
}

/// Re-hashes a cached entry against another revision's tree. Everything but the hashes is
/// carried over: the graph did not change, so neither did the dependency edges, the module
/// graph, the dependency fingerprint, or the recipes themselves.
pub fn derive(base: &HashFileData, tree: &TreeIndex) -> Result<HashFileData> {
    let recipes = base
        .partial
        .as_ref()
        .ok_or_else(|| anyhow!("base entry carries no partial-graph recipes"))?;
    let targets = base
        .hashes
        .iter()
        .map(|(label, hash)| (label.clone(), hash.kind.clone()))
        .collect();
    let hashes = evaluate(recipes, &targets, tree)?;
    Ok(HashFileData {
        hashes,
        module_graph_json: base.module_graph_json.clone(),
        dependency_fingerprint: base.dependency_fingerprint.clone(),
        dep_edges: base.dep_edges.clone(),
        partial: base.partial.clone(),
    })
}

// ---------------------------------------------------------------------------------------------
// Change classification
// ---------------------------------------------------------------------------------------------

/// One record of `git diff --raw --no-renames -z <base> <to>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffEntry {
    pub path: String,
    /// The status letter: `M`odified, `A`dded, `D`eleted, `T`ype changed, ...
    pub status: char,
    pub old_mode: String,
    pub new_mode: String,
}

/// Parses `git diff --raw --no-renames -z` records:
/// `:<old mode> <new mode> <old object> <new object> <status>\0<path>\0`.
pub fn parse_diff_raw(output: &[u8]) -> Result<Vec<DiffEntry>> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(record) = fields.next() {
        let record = String::from_utf8_lossy(record);
        let Some(record) = record.strip_prefix(':') else {
            bail!("unexpected git diff --raw record: {record}");
        };
        let mut parts = record.split(' ');
        let old_mode = parts.next().unwrap_or_default().to_owned();
        let new_mode = parts.next().unwrap_or_default().to_owned();
        let status = parts
            .nth(2)
            .and_then(|status| status.chars().next())
            .ok_or_else(|| anyhow!("unexpected git diff --raw record: :{record}"))?;
        let path = fields
            .next()
            .ok_or_else(|| anyhow!("git diff --raw record without a path: :{record}"))?;
        entries.push(DiffEntry {
            path: String::from_utf8_lossy(path).into_owned(),
            status,
            old_mode,
            new_mode,
        });
    }
    Ok(entries)
}

/// Whether Bazel reads this file while loading the graph, so a change to it can add, remove or
/// rewire targets. Matched on the basename; the list errs on the side of a full generation.
pub fn is_graph_defining_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "BUILD"
            | "BUILD.bazel"
            | "WORKSPACE"
            | "WORKSPACE.bazel"
            | "WORKSPACE.bzlmod"
            | "MODULE.bazel"
            | "MODULE.bazel.lock"
            | "REPO.bazel"
            | "VENDOR.bazel"
            | ".bazelignore"
            | ".bazelversion"
            | ".bazeliskrc"
    ) || name.ends_with(".bzl")
        || name.ends_with(".scl")
        || name.ends_with("bazelrc")
        || name.ends_with(".BUILD")
        || name.starts_with("BUILD.")
        || name.ends_with(".patch")
        || name.ends_with(".diff")
        || path == "tools/bazel"
}

/// Files that `MODULE.bazel` and the `WORKSPACE` files may name by label. Their content is
/// read while resolving external repositories (lock files, requirement files, patches), which
/// can change the graph without touching a loading-phase file.
pub const MODULE_FILES: [&str; 4] = [
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "WORKSPACE.bzlmod",
];

/// Main-repository paths named by label literals in `text`, in either quote style:
/// `"//pkg:file"`, `'@//:file'`, the root-relative `":file"` a module file may use, and the
/// shorthand `"//pkg"` for `//pkg:pkg`.
pub fn referenced_main_repo_paths(text: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for quote in ['"', '\''] {
        for (index, _) in text.match_indices(quote) {
            let rest = &text[index + 1..];
            let Some(end) = rest.find(quote) else { break };
            let literal = &rest[..end];
            if literal.contains('\n') {
                continue;
            }
            let path = if let Some(name) = literal.strip_prefix(':') {
                Some(name.replace(':', "/"))
            } else if is_main_repo(literal) {
                main_repo_path(literal).map(|path| {
                    let path = path.to_string_lossy().into_owned();
                    if literal.contains(':') {
                        path
                    } else {
                        // `//pkg/sub` names `//pkg/sub:sub`.
                        match path.rsplit('/').next() {
                            Some(last) if !path.is_empty() => format!("{path}/{last}"),
                            _ => path,
                        }
                    }
                })
            } else {
                None
            };
            if let Some(path) = path.filter(|path| !path.is_empty()) {
                paths.insert(path);
            }
        }
    }
    paths
}

/// A `--partialGraphRebuildPattern`: `*` matches within one path segment, `**` across
/// segments, `?` one character. A pattern without `/` is matched against the basename.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Glob(String);

impl Glob {
    pub fn new(pattern: &str) -> Result<Self> {
        if pattern.is_empty() {
            bail!("rebuild pattern must not be empty");
        }
        Ok(Self(pattern.to_owned()))
    }

    pub fn pattern(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, path: &str) -> bool {
        let subject = if self.0.contains('/') {
            path
        } else {
            path.rsplit('/').next().unwrap_or(path)
        };
        glob_match(self.0.as_bytes(), subject.as_bytes())
    }
}

fn glob_match(pattern: &[u8], subject: &[u8]) -> bool {
    match pattern {
        [] => subject.is_empty(),
        [b'*', b'*', rest @ ..] => {
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            (0..=subject.len()).any(|skip| {
                (skip == 0 || subject[skip - 1] == b'/' || skip == subject.len())
                    && glob_match(rest, &subject[skip..])
            }) || glob_match(rest, subject)
        }
        [b'*', rest @ ..] => (0..=subject.len())
            .take_while(|skip| *skip == 0 || subject[skip - 1] != b'/')
            .any(|skip| glob_match(rest, &subject[skip..])),
        [b'?', rest @ ..] => match subject {
            [first, tail @ ..] if *first != b'/' => glob_match(rest, tail),
            _ => false,
        },
        [literal, rest @ ..] => match subject {
            [first, tail @ ..] if first == literal => glob_match(rest, tail),
            _ => false,
        },
    }
}

/// What the classifier concluded about a revision pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Classification {
    /// Every changed path is a content change to a regular file; the labels are the source
    /// targets those paths map to, and `unmatched` the changed paths no target reads.
    Derivable {
        changed_sources: Vec<String>,
        unmatched_paths: Vec<String>,
    },
    /// The graph may have changed; `reason` names the first path that forced the decision.
    Rebuild { reason: String },
}

/// The inputs to [`classify`] that come from git and the server configuration.
pub struct ClassifyInputs<'a> {
    pub diff: &'a [DiffEntry],
    /// The revision being hashed.
    pub tree: &'a TreeIndex,
    pub base: &'a HashFileData,
    /// The workspace root, against which seed paths are normalized.
    pub workspace: &'a Path,
    pub seed_filepaths: &'a BTreeSet<PathBuf>,
    pub rebuild_patterns: &'a [Glob],
    /// Paths named by label in the module and workspace files (see
    /// [`referenced_main_repo_paths`]).
    pub module_referenced: &'a BTreeSet<String>,
}

pub fn classify(inputs: &ClassifyInputs<'_>) -> Classification {
    let rebuild = |reason: String| Classification::Rebuild { reason };
    let Some(recipes) = inputs.base.partial.as_ref() else {
        return rebuild("base entry carries no partial-graph recipes".to_owned());
    };
    for path in inputs.tree.symlinks() {
        if is_graph_defining_path(path) {
            return rebuild(format!("symlinked graph-defining file {path}"));
        }
    }
    let seeds = inputs
        .seed_filepaths
        .iter()
        .filter_map(|path| normalize_workspace_path(inputs.workspace, &path.to_string_lossy()))
        .collect::<BTreeSet<_>>();
    let mut changed_sources = Vec::new();
    let mut unmatched_paths = Vec::new();
    for entry in inputs.diff {
        let path = entry.path.as_str();
        if entry.status != 'M' {
            return rebuild(format!(
                "{path} was {} (status {})",
                match entry.status {
                    'A' => "added",
                    'D' => "deleted",
                    'T' => "type-changed",
                    'R' => "renamed",
                    'C' => "copied",
                    _ => "changed",
                },
                entry.status
            ));
        }
        if !is_regular_mode(&entry.old_mode) || !is_regular_mode(&entry.new_mode) {
            return rebuild(format!(
                "{path} is not a regular file on both sides (modes {} -> {})",
                entry.old_mode, entry.new_mode
            ));
        }
        if is_graph_defining_path(path) {
            return rebuild(format!("graph-defining file {path} changed"));
        }
        if inputs.module_referenced.contains(path) {
            return rebuild(format!(
                "{path} is named by a module or workspace file and changed"
            ));
        }
        if seeds.contains(path) {
            return rebuild(format!("seed file {path} changed"));
        }
        if let Some(input) = recipes
            .load_inputs
            .iter()
            .find(|input| path == input.as_str() || path.starts_with(&format!("{input}/")))
        {
            return rebuild(format!(
                "{path} is read while loading the graph (under {input}) and changed"
            ));
        }
        if let Some(glob) = inputs
            .rebuild_patterns
            .iter()
            .find(|glob| glob.matches(path))
        {
            return rebuild(format!("{path} matches rebuild pattern {}", glob.pattern()));
        }
        let labels = source_labels_for_path(&inputs.base.hashes, path);
        if labels.is_empty() {
            unmatched_paths.push(path.to_owned());
        } else {
            changed_sources.extend(labels);
        }
    }
    Classification::Derivable {
        changed_sources,
        unmatched_paths,
    }
}

fn is_regular_mode(mode: &str) -> bool {
    matches!(mode, "100644" | "100664" | "100755")
}

/// The source targets whose content a change to `path` reaches: the label naming the path
/// itself under any package split, and any ancestor directory that is a source target.
pub fn source_labels_for_path(hashes: &BTreeMap<String, TargetHash>, path: &str) -> Vec<String> {
    let components = path.split('/').collect::<Vec<_>>();
    let mut labels = Vec::new();
    for length in (1..=components.len()).rev() {
        let prefix = &components[..length];
        for split in 0..length {
            let label = format!(
                "//{}:{}",
                prefix[..split].join("/"),
                prefix[split..].join("/")
            );
            if hashes
                .get(&label)
                .is_some_and(|hash| hash.kind == "SourceFile")
            {
                labels.push(label);
            }
        }
    }
    labels
}

/// The main-repository path a source label names, for callers that map the other way.
pub fn label_path(label: &str) -> Option<String> {
    main_repo_path(target_label(label)).map(|path| path.to_string_lossy().into_owned())
}

pub fn workspace_relative(workspace: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(workspace)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(records: &[(&str, &str, &str)]) -> TreeIndex {
        let mut output = Vec::new();
        for (mode, object, path) in records {
            let object_type = if *mode == "160000" { "commit" } else { "blob" };
            output.extend_from_slice(format!("{mode} {object_type} {object}\t{path}").as_bytes());
            output.push(0);
        }
        TreeIndex::parse_ls_tree(&output)
    }

    fn hash(kind: &str) -> TargetHash {
        TargetHash {
            kind: kind.into(),
            hash: "h".into(),
            direct_hash: "d".into(),
        }
    }

    fn base(labels: &[(&str, &str)], recipes: bool) -> HashFileData {
        HashFileData {
            hashes: labels
                .iter()
                .map(|(label, kind)| ((*label).to_owned(), hash(kind)))
                .collect(),
            partial: recipes.then(PartialRecipes::default),
            ..Default::default()
        }
    }

    #[test]
    fn tree_index_parses_modes_and_lists_directories() {
        let index = tree(&[
            ("100644", "aaaa", "pkg/a.txt"),
            ("100755", "bbbb", "pkg/run.sh"),
            ("120000", "cccc", "pkg/link"),
            ("160000", "dddd", "vendor/sub"),
            ("040000", "eeee", "pkg"),
            ("100644", "ffff", "pkgx/other.txt"),
        ]);
        assert_eq!(index.len(), 5, "the tree record is skipped");
        assert_eq!(
            index.get("pkg/a.txt").unwrap().kind,
            TreeKind::File { executable: false }
        );
        assert_eq!(
            index.get("pkg/run.sh").unwrap().kind,
            TreeKind::File { executable: true }
        );
        assert_eq!(index.get("pkg/link").unwrap().kind, TreeKind::Symlink);
        assert_eq!(index.get("vendor/sub").unwrap().kind, TreeKind::Submodule);
        assert!(index.get("pkg").is_none());
        let under = index
            .under("pkg")
            .map(|(path, _)| path.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(under, ["a.txt", "link", "run.sh"]);
        assert_eq!(index.under("pkg/").count(), 3);
        assert!(index.under("nope").next().is_none());
        assert_eq!(index.symlinks().collect::<Vec<_>>(), ["pkg/link"]);
        let content = index.content_hashes();
        assert_eq!(content.len(), 4);
        assert_eq!(content["pkg/link"], "cccc");
        assert!(!content.contains_key("vendor/sub"));
        assert!(!index.is_empty());
        assert!(TreeIndex::parse_ls_tree(b"garbage\0").is_empty());
    }

    #[test]
    fn source_digests_depend_on_content_mode_kind_seed_and_label() {
        let index = tree(&[
            ("100644", "aaaa", "pkg/a.txt"),
            ("100755", "aaaa", "pkg/exec.txt"),
            ("100644", "aaaa", "pkg/same.txt"),
            ("120000", "aaaa", "pkg/link"),
            ("160000", "aaaa", "pkg/sub"),
            ("100644", "d1", "pkg/dir/one"),
            ("100644", "d2", "pkg/dir/two"),
        ]);
        let a = source_digest(&index, "//pkg:a.txt", b"seed");
        assert_ne!(a, source_digest(&index, "//pkg:a.txt", b"other"));
        assert_ne!(a, source_digest(&index, "//pkg:same.txt", b"seed"));
        assert_ne!(
            source_digest(&index, "//pkg:exec.txt", b"s"),
            source_digest(&index, "//pkg:a.txt", b"s")
        );
        assert_ne!(
            source_digest(&index, "//pkg:link", b"s"),
            source_digest(&index, "//pkg:sub", b"s")
        );
        let directory = source_digest(&index, "//pkg:dir", b"s");
        let mut changed = tree(&[
            ("100644", "d1", "pkg/dir/one"),
            ("100644", "d3", "pkg/dir/two"),
        ]);
        assert_ne!(directory, source_digest(&changed, "//pkg:dir", b"s"));
        changed = tree(&[
            ("100644", "d1", "pkg/dir/one"),
            ("100755", "d2", "pkg/dir/two"),
        ]);
        assert_ne!(directory, source_digest(&changed, "//pkg:dir", b"s"));
        let missing = source_digest(&index, "//pkg:missing", b"s");
        assert_ne!(missing, directory);
        assert_eq!(
            source_digest(&index, "@ext//:file", b"s"),
            <DigestBytes>::from(Sha256::digest([])),
            "external labels digest nothing, as without fine-grained repositories"
        );
        assert_ne!(
            source_digest(&index, "@//pkg:a.txt", b"seed"),
            source_digest(&index, "//pkg:a.txt", b"seed"),
            "the label spelling is part of the digest, as in the default mode"
        );

        assert!(soft_digest(&index, "//pkg:a.txt").is_some());
        assert!(soft_digest(&index, "//pkg:link").is_some());
        assert!(soft_digest(&index, "//pkg:sub").is_none());
        assert!(soft_digest(&index, "//pkg:dir").is_none());
        assert!(soft_digest(&index, "//pkg:missing").is_none());
        assert!(soft_digest(&index, "@ext//:x").is_none());
    }

    fn recipes() -> PartialRecipes {
        let mut recorder = RecipeRecorder::new([7; 32], true);
        recorder.record_source("//pkg:a.txt", &["//tools:defs.bzl".into()]);
        recorder.record_source("//pkg:b.txt", &["//tools:defs.bzl".into()]);
        recorder.record_source("//pkg:odd.txt", &[]);
        recorder.record_source("//other:c.txt", &[]);
        recorder.record_source("//other:d.txt", &["//tools:late.bzl".into()]);
        recorder.record_rule(
            "//pkg:lib",
            RuleRecipe::new([1; 32], vec!["//pkg:a.txt".into()], vec![]),
        );
        recorder.record_rule(
            "//pkg:bin",
            RuleRecipe::new(
                [2; 32],
                vec!["//pkg:b.txt".into(), "//pkg:soft.txt".into()],
                vec!["//pkg:lib".into()],
            ),
        );
        recorder.record_rule(
            "//cycle:x",
            RuleRecipe::new([3; 32], vec![], vec!["//cycle:y".into()]),
        );
        recorder.record_rule(
            "//cycle:y",
            RuleRecipe::new([4; 32], vec![], vec!["//cycle:x".into()]),
        );
        recorder.record_generated("//pkg:bin.out", "//pkg:bin");
        recorder.finish()
    }

    fn targets() -> BTreeMap<String, String> {
        [
            ("//pkg:a.txt", "SourceFile"),
            ("//pkg:b.txt", "SourceFile"),
            ("//pkg:odd.txt", "SourceFile"),
            ("//other:c.txt", "SourceFile"),
            ("//other:d.txt", "SourceFile"),
            ("//pkg:lib", "Rule"),
            ("//pkg:bin", "Rule"),
            ("//pkg:bin.out", "GeneratedFile"),
            ("//cycle:x", "Rule"),
            ("//cycle:y", "Rule"),
        ]
        .into_iter()
        .map(|(label, kind)| (label.to_owned(), kind.to_owned()))
        .collect()
    }

    fn fixture_tree() -> TreeIndex {
        tree(&[
            ("100644", "a1", "pkg/a.txt"),
            ("100644", "b1", "pkg/b.txt"),
            ("100644", "o1", "pkg/odd.txt"),
            ("100644", "s1", "pkg/soft.txt"),
            ("100644", "c1", "other/c.txt"),
            ("100644", "d1", "other/d.txt"),
            ("100644", "t1", "tools/defs.bzl"),
        ])
    }

    #[test]
    fn recorder_stores_package_subincludes_with_per_source_overrides() {
        let recipes = recipes();
        // The same sources recorded in another order yield the same recipes.
        let mut reversed = RecipeRecorder::new([7; 32], true);
        reversed.record_source("//other:d.txt", &["//tools:late.bzl".into()]);
        reversed.record_source("//other:c.txt", &[]);
        reversed.record_source("//pkg:odd.txt", &[]);
        reversed.record_source("//pkg:b.txt", &["//tools:defs.bzl".into()]);
        reversed.record_source("//pkg:a.txt", &["//tools:defs.bzl".into()]);
        for (key, recipe) in &recipes.rules {
            reversed.record_rule(key, recipe.clone());
        }
        reversed.record_generated("//pkg:bin.out", "//pkg:bin");
        assert_eq!(reversed.finish(), recipes);
        assert_eq!(recipes.version, RECIPE_VERSION);
        assert_eq!(recipes.seed_hash, hex::encode([7; 32]));
        assert_eq!(
            recipes.package_subincludes,
            BTreeMap::from([("//pkg".to_owned(), vec!["//tools:defs.bzl".to_owned()])])
        );
        assert_eq!(
            recipes.source_subincludes,
            BTreeMap::from([
                ("//pkg:odd.txt".to_owned(), vec![]),
                (
                    "//other:d.txt".to_owned(),
                    vec!["//tools:late.bzl".to_owned()]
                ),
            ])
        );
        assert_eq!(recipes.subincludes("//pkg:b.txt"), ["//tools:defs.bzl"]);
        assert!(recipes.subincludes("//pkg:odd.txt").is_empty());
        assert!(recipes.subincludes("//other:c.txt").is_empty());
        assert_eq!(recipes.subincludes("//other:d.txt"), ["//tools:late.bzl"]);
        let json = serde_json::to_string(&recipes).unwrap();
        let round_trip: PartialRecipes = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, recipes);
        assert!(json.contains(r#""//pkg:bin":["0202"#));
        assert_eq!(recipes.rules["//pkg:bin"].static_digest().unwrap(), [2; 32]);
        assert!(RuleRecipe("zz".into(), vec![], vec![])
            .static_digest()
            .is_err());
        assert!(RuleRecipe("00".into(), vec![], vec![])
            .static_digest()
            .is_err());
    }

    #[test]
    fn evaluation_is_deterministic_and_propagates_source_changes() {
        let recipes = recipes();
        let targets = targets();
        let before = evaluate(&recipes, &targets, &fixture_tree()).unwrap();
        assert_eq!(before.len(), targets.len());
        assert_eq!(
            before,
            evaluate(&recipes, &targets, &fixture_tree()).unwrap()
        );
        assert_eq!(before["//pkg:bin.out"].hash, before["//pkg:bin"].hash);
        assert_eq!(before["//pkg:a.txt"].kind, "SourceFile");
        assert_eq!(
            before["//pkg:a.txt"].hash,
            before["//pkg:a.txt"].direct_hash
        );
        assert_ne!(before["//cycle:x"].hash, before["//cycle:y"].hash);

        let mut edited = fixture_tree();
        edited.entries.get_mut("pkg/a.txt").unwrap().object = "a2".into();
        let after = evaluate(&recipes, &targets, &edited).unwrap();
        let changed = before
            .iter()
            .filter(|(label, hash)| !after[*label].same_as(hash))
            .map(|(label, _)| label.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            changed,
            BTreeSet::from(["//pkg:a.txt", "//pkg:lib", "//pkg:bin", "//pkg:bin.out"])
        );
        assert_ne!(
            before["//pkg:lib"].direct_hash,
            after["//pkg:lib"].direct_hash
        );
        assert_eq!(
            before["//pkg:bin"].direct_hash, after["//pkg:bin"].direct_hash,
            "a dependency's change is transitive, not direct"
        );

        let mut soft = fixture_tree();
        soft.entries.get_mut("pkg/soft.txt").unwrap().object = "s2".into();
        let after = evaluate(&recipes, &targets, &soft).unwrap();
        assert_ne!(
            before["//pkg:bin"].direct_hash,
            after["//pkg:bin"].direct_hash
        );
        assert_eq!(before["//pkg:lib"].hash, after["//pkg:lib"].hash);

        let mut bzl = fixture_tree();
        bzl.entries.get_mut("tools/defs.bzl").unwrap().object = "t2".into();
        let after = evaluate(&recipes, &targets, &bzl).unwrap();
        assert_ne!(before["//pkg:a.txt"].hash, after["//pkg:a.txt"].hash);
        assert_eq!(before["//other:c.txt"].hash, after["//other:c.txt"].hash);
        let mut no_package_seeds = recipes.clone();
        no_package_seeds.package_seeds = false;
        assert_eq!(
            evaluate(&no_package_seeds, &targets, &fixture_tree()).unwrap()["//pkg:a.txt"].hash,
            evaluate(&no_package_seeds, &targets, &bzl).unwrap()["//pkg:a.txt"].hash
        );
    }

    #[test]
    fn evaluation_rejects_incomplete_recipes() {
        let recipes = recipes();
        let mut targets = targets();
        let mut missing_soft = fixture_tree();
        missing_soft.entries.remove("pkg/soft.txt");
        let error = evaluate(&recipes, &targets, &missing_soft)
            .unwrap_err()
            .to_string();
        assert!(error.contains("//pkg:soft.txt"), "{error}");

        targets.insert("//pkg:unknown".into(), "Rule".into());
        assert!(evaluate(&recipes, &targets, &fixture_tree())
            .unwrap_err()
            .to_string()
            .contains("//pkg:unknown"));
        targets.remove("//pkg:unknown");
        targets.insert("//pkg:orphan.out".into(), "GeneratedFile".into());
        assert!(evaluate(&recipes, &targets, &fixture_tree())
            .unwrap_err()
            .to_string()
            .contains("//pkg:orphan.out"));
        targets.remove("//pkg:orphan.out");
        targets.insert("//pkg:weird".into(), "PackageGroup".into());
        assert!(evaluate(&recipes, &targets, &fixture_tree()).is_err());
        targets.remove("//pkg:weird");

        let mut old = recipes.clone();
        old.version = 0;
        assert!(evaluate(&old, &targets, &fixture_tree())
            .unwrap_err()
            .to_string()
            .contains("format version 0"));
        let mut bad_seed = recipes;
        bad_seed.seed_hash = "nope".into();
        assert!(evaluate(&bad_seed, &targets, &fixture_tree()).is_err());
    }

    #[test]
    fn derive_carries_metadata_and_requires_recipes() {
        let recipes = recipes();
        let targets = targets();
        let mut base = HashFileData {
            hashes: evaluate(&recipes, &targets, &fixture_tree()).unwrap(),
            module_graph_json: Some("{}".into()),
            dependency_fingerprint: Some("fp".into()),
            dep_edges: BTreeMap::from([("//pkg:bin".to_owned(), vec!["//pkg:lib".to_owned()])]),
            partial: Some(recipes),
        };
        let mut edited = fixture_tree();
        edited.entries.get_mut("other/c.txt").unwrap().object = "c2".into();
        let derived = derive(&base, &edited).unwrap();
        assert_eq!(derived.module_graph_json.as_deref(), Some("{}"));
        assert_eq!(derived.dependency_fingerprint.as_deref(), Some("fp"));
        assert_eq!(derived.dep_edges, base.dep_edges);
        assert_eq!(derived.partial, base.partial);
        assert_ne!(
            derived.hashes["//other:c.txt"].hash,
            base.hashes["//other:c.txt"].hash
        );
        assert_eq!(
            derived.hashes["//pkg:bin"].hash,
            base.hashes["//pkg:bin"].hash
        );
        assert_eq!(derive(&base, &fixture_tree()).unwrap().hashes, base.hashes);
        base.partial = None;
        assert!(derive(&base, &edited).is_err());
    }

    #[test]
    fn parses_raw_diff_records() {
        let output = b":100644 100644 aaaa bbbb M\0pkg/a.txt\0:000000 100644 0000 cccc A\0new file\0:100644 100755 dddd dddd M\0pkg/run.sh\0";
        let entries = parse_diff_raw(output).unwrap();
        assert_eq!(
            entries,
            [
                DiffEntry {
                    path: "pkg/a.txt".into(),
                    status: 'M',
                    old_mode: "100644".into(),
                    new_mode: "100644".into(),
                },
                DiffEntry {
                    path: "new file".into(),
                    status: 'A',
                    old_mode: "000000".into(),
                    new_mode: "100644".into(),
                },
                DiffEntry {
                    path: "pkg/run.sh".into(),
                    status: 'M',
                    old_mode: "100644".into(),
                    new_mode: "100755".into(),
                },
            ]
        );
        assert!(parse_diff_raw(b"").unwrap().is_empty());
        assert!(parse_diff_raw(b"junk\0path\0").is_err());
        assert!(parse_diff_raw(b":100644 100644\0path\0").is_err());
        assert!(parse_diff_raw(b":100644 100644 a b M\0").is_err());
    }

    #[test]
    fn graph_defining_paths_cover_loading_phase_files() {
        for path in [
            "BUILD",
            "pkg/BUILD.bazel",
            "WORKSPACE",
            "WORKSPACE.bazel",
            "WORKSPACE.bzlmod",
            "MODULE.bazel",
            "MODULE.bazel.lock",
            "sub/REPO.bazel",
            "VENDOR.bazel",
            ".bazelignore",
            ".bazelversion",
            ".bazeliskrc",
            ".bazelrc",
            "user.bazelrc",
            "ci/remote.bazelrc",
            "tools/defs.bzl",
            "tools/defs.scl",
            "third_party/foo.BUILD",
            "third_party/BUILD.foo.bazel",
            "third_party/fix.patch",
            "third_party/fix.diff",
            "tools/bazel",
        ] {
            assert!(is_graph_defining_path(path), "{path}");
        }
        for path in [
            "pkg/main.rs",
            "pkg/build.rs",
            "docs/BUILDING.md",
            "pkg/bazel.txt",
            "tools/bazelisk",
            "x/tools/bazel",
            "pkg/notbzl.txt",
        ] {
            assert!(!is_graph_defining_path(path), "{path}");
        }
    }

    #[test]
    fn module_file_scan_finds_main_repo_labels() {
        let text = r#"
            module(name = "x")
            pip.parse(requirements_lock = "//:requirements.txt")
            crate.from_cargo(cargo_lockfile = "@//third_party:Cargo.lock", manifests = ["//:Cargo.toml"])
            single_version_override(patches = ["//patches:fix.patch"], version = "1.0")
            bazel_dep(name = "rules_cc", version = "0.1")  # "//" inside a comment line
            not_a_label = "@rules_cc//cc:defs.bzl"
            multi = "//a
            b"
            single = '//tools:versions.json'
            relative = ":go.mod"
            shorthand = "//third_party/boringssl"
        "#;
        assert_eq!(
            referenced_main_repo_paths(text),
            BTreeSet::from([
                "requirements.txt".to_owned(),
                "third_party/Cargo.lock".to_owned(),
                "Cargo.toml".to_owned(),
                "patches/fix.patch".to_owned(),
                "tools/versions.json".to_owned(),
                "go.mod".to_owned(),
                "third_party/boringssl/boringssl".to_owned(),
            ])
        );
        assert!(referenced_main_repo_paths("no labels here").is_empty());
        assert!(referenced_main_repo_paths("\"//\"").is_empty());
    }

    #[test]
    fn globs_match_segments_and_basenames() {
        let cases = [
            ("*.json", "a/b/c.json", true),
            ("*.json", "a/b/c.yaml", false),
            ("c?json", "x/c.json", true),
            ("c?json", "x/c/json", false),
            ("tools/*.json", "tools/a.json", true),
            ("tools/*.json", "tools/sub/a.json", false),
            ("tools/**/*.json", "tools/sub/deep/a.json", true),
            ("tools/**/*.json", "tools/a.json", true),
            ("**/lock", "lock", true),
            ("**/lock", "a/b/lock", true),
            ("**/lock", "a/b/locked", false),
            ("a/**", "a/b/c", true),
            ("a/**", "b/c", false),
            ("exact/path.txt", "exact/path.txt", true),
            ("exact/path.txt", "exact/path.txt2", false),
            ("*", "a/b", true),
            ("a/*", "a/b/c", false),
        ];
        for (pattern, path, expected) in cases {
            assert_eq!(
                Glob::new(pattern).unwrap().matches(path),
                expected,
                "{pattern} vs {path}"
            );
        }
        assert!(Glob::new("").is_err());
        assert_eq!(Glob::new("x").unwrap().pattern(), "x");
    }

    #[test]
    fn maps_paths_to_source_targets_under_any_package_split() {
        let hashes = base(
            &[
                ("//pkg:a.txt", "SourceFile"),
                ("//:top/b.txt", "SourceFile"),
                ("//pkg:testdata", "SourceFile"),
                ("//pkg:rule", "Rule"),
                ("//pkg/sub:deep/c.txt", "SourceFile"),
            ],
            true,
        )
        .hashes;
        assert_eq!(
            source_labels_for_path(&hashes, "pkg/a.txt"),
            ["//pkg:a.txt"]
        );
        assert_eq!(
            source_labels_for_path(&hashes, "top/b.txt"),
            ["//:top/b.txt"]
        );
        assert_eq!(
            source_labels_for_path(&hashes, "pkg/sub/deep/c.txt"),
            ["//pkg/sub:deep/c.txt"]
        );
        assert_eq!(
            source_labels_for_path(&hashes, "pkg/testdata/inner/file"),
            ["//pkg:testdata"]
        );
        assert!(source_labels_for_path(&hashes, "pkg/rule").is_empty());
        assert!(source_labels_for_path(&hashes, "README.md").is_empty());
        assert_eq!(label_path("//pkg:a.txt").as_deref(), Some("pkg/a.txt"));
        assert_eq!(label_path("//pkg:a.txt|abc").as_deref(), Some("pkg/a.txt"));
        assert!(label_path("@ext//:x").is_none());
        assert_eq!(
            workspace_relative(Path::new("/ws"), Path::new("/ws/a/b")).as_deref(),
            Some("a/b")
        );
        assert!(workspace_relative(Path::new("/ws"), Path::new("/other")).is_none());
    }

    fn entry(path: &str, status: char, old: &str, new: &str) -> DiffEntry {
        DiffEntry {
            path: path.into(),
            status,
            old_mode: old.into(),
            new_mode: new.into(),
        }
    }

    fn classify_with(diff: &[DiffEntry], tree: &TreeIndex, base: &HashFileData) -> Classification {
        classify(&ClassifyInputs {
            diff,
            tree,
            base,
            workspace: Path::new("/ws"),
            seed_filepaths: &BTreeSet::from([
                PathBuf::from("./seed.txt"),
                PathBuf::from("/ws/config/../other.seed"),
                PathBuf::from("/elsewhere/ignored.seed"),
            ]),
            rebuild_patterns: &[Glob::new("*.lock").unwrap()],
            module_referenced: &BTreeSet::from(["requirements.txt".to_owned()]),
        })
    }

    #[test]
    fn classifier_derives_only_content_changes_to_known_regular_files() {
        let base_data = base(
            &[("//pkg:a.txt", "SourceFile"), ("//pkg:lib", "Rule")],
            true,
        );
        let no_recipes = base(&[], false);
        let base = &base_data;
        let index = tree(&[("100644", "a", "pkg/a.txt"), ("100644", "r", "README.md")]);
        let tree_ref = &index;
        let modified = |path: &str| entry(path, 'M', "100644", "100644");
        assert_eq!(
            classify_with(
                &[modified("pkg/a.txt"), modified("README.md")],
                tree_ref,
                base
            ),
            Classification::Derivable {
                changed_sources: vec!["//pkg:a.txt".into()],
                unmatched_paths: vec!["README.md".into()],
            }
        );
        assert_eq!(
            classify_with(&[], tree_ref, base),
            Classification::Derivable {
                changed_sources: vec![],
                unmatched_paths: vec![],
            }
        );
        assert_eq!(
            classify_with(
                &[entry("pkg/a.txt", 'M', "100644", "100755")],
                tree_ref,
                base
            ),
            Classification::Derivable {
                changed_sources: vec!["//pkg:a.txt".into()],
                unmatched_paths: vec![],
            },
            "an executable-bit flip is a content change the recipe captures"
        );

        let reason = |diff: &[DiffEntry], tree: &TreeIndex, base: &HashFileData| match classify_with(
            diff, tree, base,
        ) {
            Classification::Rebuild { reason } => reason,
            other => panic!("expected a rebuild, got {other:?}"),
        };
        assert!(reason(
            &[entry("pkg/new.txt", 'A', "000000", "100644")],
            tree_ref,
            base
        )
        .contains("added"));
        assert!(reason(
            &[entry("pkg/a.txt", 'D', "100644", "000000")],
            tree_ref,
            base
        )
        .contains("deleted"));
        assert!(reason(
            &[entry("pkg/a.txt", 'T', "100644", "120000")],
            tree_ref,
            base
        )
        .contains("type-changed"));
        assert!(reason(
            &[entry("pkg/a.txt", 'R', "100644", "100644")],
            tree_ref,
            base
        )
        .contains("renamed"));
        assert!(reason(
            &[entry("pkg/a.txt", 'C', "100644", "100644")],
            tree_ref,
            base
        )
        .contains("copied"));
        assert!(reason(
            &[entry("pkg/a.txt", 'X', "100644", "100644")],
            tree_ref,
            base
        )
        .contains("status X"));
        assert!(reason(
            &[entry("pkg/link", 'M', "120000", "120000")],
            tree_ref,
            base
        )
        .contains("not a regular file"));
        assert!(reason(&[modified("pkg/BUILD.bazel")], tree_ref, base).contains("graph-defining"));
        assert!(reason(&[modified("requirements.txt")], tree_ref, base)
            .contains("named by a module or workspace file"));
        assert!(reason(&[modified("seed.txt")], tree_ref, base).contains("seed file"));
        assert!(reason(&[modified("other.seed")], tree_ref, base).contains("seed file"));
        let mut with_inputs = base_data.clone();
        with_inputs.partial.as_mut().unwrap().load_inputs =
            BTreeSet::from(["dep_repo".to_owned(), "versions.json".to_owned()]);
        assert!(
            reason(&[modified("dep_repo/hello")], tree_ref, &with_inputs)
                .contains("read while loading the graph (under dep_repo)")
        );
        assert!(reason(&[modified("versions.json")], tree_ref, &with_inputs)
            .contains("read while loading"));
        assert!(matches!(
            classify_with(&[modified("dep_repository/x")], tree_ref, &with_inputs),
            Classification::Derivable { .. }
        ));
        assert!(
            reason(&[modified("deps/foo.lock")], tree_ref, base).contains("rebuild pattern *.lock")
        );
        assert!(reason(&[modified("pkg/a.txt")], tree_ref, &no_recipes)
            .contains("no partial-graph recipes"));
        let linked = tree(&[("120000", "l", "pkg/BUILD"), ("100644", "a", "pkg/a.txt")]);
        assert!(reason(&[modified("pkg/a.txt")], &linked, base).contains("symlinked"));
        let harmless_link = tree(&[("120000", "l", "pkg/link.txt")]);
        assert!(matches!(
            classify_with(&[modified("pkg/a.txt")], &harmless_link, base),
            Classification::Derivable { .. }
        ));
    }
}

#[cfg(test)]
mod loading_input_tests {
    use super::*;
    use crate::proto::blaze_query::{attribute, Attribute};

    fn string_attribute(name: &str, value: &str) -> Attribute {
        Attribute {
            name: name.to_owned(),
            r#type: attribute::Discriminator::String,
            string_value: Some(value.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn normalizes_workspace_paths_like_git_names_them() {
        let workspace = Path::new("/ws");
        for (value, expected) in [
            ("seed.txt", Some("seed.txt")),
            ("./seed.txt", Some("seed.txt")),
            ("config/../seed.txt", Some("seed.txt")),
            ("a/./b//c", Some("a/b/c")),
            ("%workspace%/dep_repo", Some("dep_repo")),
            ("%workspace%", Some("")),
            ("/ws/dep_repo/sub", Some("dep_repo/sub")),
            ("/elsewhere/file", None),
            ("../outside", None),
        ] {
            assert_eq!(
                normalize_workspace_path(workspace, value).as_deref(),
                expected,
                "{value}"
            );
        }
        let real = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(real.path()).unwrap();
        assert_eq!(
            normalize_workspace_path(real.path(), &format!("{}/x/y", canonical.display()))
                .as_deref(),
            Some("x/y"),
            "the canonical spelling of the workspace is accepted too"
        );
    }

    #[test]
    fn repository_rules_name_their_loading_inputs() {
        let workspace = Path::new("/ws");
        let local = Rule {
            name: "//external:dep_repo".into(),
            rule_class: "local_repository".into(),
            attribute: vec![
                string_attribute("path", "%workspace%/dep_repo"),
                string_attribute("build_file", "//third_party:dep.BUILD"),
                string_attribute("name", "dep_repo"),
            ],
            ..Default::default()
        };
        assert_eq!(
            rule_loading_inputs(&local, workspace),
            ["dep_repo", "third_party/dep.BUILD"]
        );
        let outside = Rule {
            name: "//external:elsewhere".into(),
            rule_class: "local_repository".into(),
            attribute: vec![string_attribute("path", "/opt/elsewhere")],
            ..Default::default()
        };
        assert!(rule_loading_inputs(&outside, workspace).is_empty());
        let archive = Rule {
            name: "//external:archive".into(),
            rule_class: "http_archive".into(),
            attribute: vec![
                string_attribute("path", "ignored/for/non-local"),
                string_attribute("patches", "//patches:fix.patch"),
            ],
            ..Default::default()
        };
        assert_eq!(
            rule_loading_inputs(&archive, workspace),
            ["patches/fix.patch"]
        );
        let ordinary = Rule {
            name: "//pkg:lib".into(),
            rule_class: "local_repository".into(),
            attribute: vec![string_attribute("path", "pkg")],
            ..Default::default()
        };
        assert!(rule_loading_inputs(&ordinary, workspace).is_empty());
    }

    #[test]
    fn marker_files_list_the_main_repo_files_a_fetch_read() {
        let text = "abc123\nFILE:@@//requirements.txt 0000\nFILE:@@rules_python+//python/x.bzl 1111\nFILE:@//:versions.json 2222\nENV:HOME x\nFILE://pkg:data 3333\n";
        assert_eq!(
            marker_file_inputs(text).collect::<Vec<_>>(),
            ["requirements.txt", "versions.json", "pkg/data"]
        );
        let output_base = tempfile::tempdir().unwrap();
        assert!(marker_loading_inputs(output_base.path()).is_empty());
        let external = output_base.path().join("external");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("@pip+.marker"), text).unwrap();
        fs::write(external.join("@other+.marker"), "FILE:@@//go.mod 4444\n").unwrap();
        fs::write(external.join("not-a-marker.txt"), "FILE:@@//ignored 5555\n").unwrap();
        assert_eq!(
            marker_loading_inputs(output_base.path()),
            BTreeSet::from([
                "requirements.txt".to_owned(),
                "versions.json".to_owned(),
                "pkg/data".to_owned(),
                "go.mod".to_owned(),
            ])
        );
    }
}
