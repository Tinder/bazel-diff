use anyhow::{bail, Context, Result};
use bazel_diff::bazel::BazelOptions;
use bazel_diff::explain::{self, ExplainFormat, ExplainLimits};
use bazel_diff::fingerprint;
use bazel_diff::git::{self, DerivationPolicy, Git};
use bazel_diff::hash::{generate_hashes, HashOptions};
use bazel_diff::model::{
    filter_and_sort_labels, impacted_targets, impacted_targets_with_distances, HashFileData,
};
use bazel_diff::module_graph::impacted_with_module_changes;
use bazel_diff::partial::{self, Classification, Glob};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Use mimalloc's per-thread heaps for parallel protobuf decoding in generate-hashes.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(
    name = "bazel-diff",
    version,
    propagate_version = true,
    about = "Writes impacted targets between two Bazel graph hash files"
)]
struct Cli {
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(
        name = "generate-hashes",
        about = "Write canonical hashes for Bazel targets in a workspace"
    )]
    GenerateHashes(GenerateHashesArgs),
    #[command(
        name = "get-impacted-targets",
        about = "Compare two hash files and report impacted targets"
    )]
    GetImpactedTargets(GetImpactedTargetsArgs),
    #[command(
        about = "Explain why a target was impacted: the upstream target(s) whose own hash \
                 changed, and the dependency path from each down to the queried target. \
                 Renders as text, JSON, Graphviz DOT, or a Mermaid flowchart."
    )]
    Explain(ExplainArgs),
    #[command(about = "Warm Bazel and write snapshot hashes and fingerprint metadata")]
    Warmup(WarmupArgs),
    #[command(about = "Compute the snapshot/cache fingerprint for the current workspace")]
    Fingerprint(FingerprintArgs),
    #[command(about = "Run the HTTP impacted-target query service")]
    Serve(Box<ServeArgs>),
}

#[derive(Clone, Debug, Args)]
struct HashingArgs {
    #[arg(
        short = 'w',
        long = "workspacePath",
        value_parser = parse_normalized_path,
        help = "Path to the Bazel workspace"
    )]
    workspace_path: PathBuf,

    #[arg(
        short = 'b',
        long = "bazelPath",
        default_value = "bazel",
        help = "Path to the Bazel or Bazelisk executable"
    )]
    bazel_path: PathBuf,

    #[arg(
        short = 's',
        long = "seed-filepaths",
        help = "File containing workspace-relative paths whose contents seed every target hash"
    )]
    seed_filepaths: Option<PathBuf>,

    #[arg(
        long = "bazelStartupOptions",
        allow_hyphen_values = true,
        help = "Additional space-separated Bazel startup options"
    )]
    bazel_startup_options: Vec<String>,

    #[arg(
        long = "bazelCommandOptions",
        allow_hyphen_values = true,
        help = "Additional space-separated `bazel query` options"
    )]
    bazel_command_options: Vec<String>,

    #[arg(
        long = "cqueryCommandOptions",
        allow_hyphen_values = true,
        help = "Additional space-separated `bazel cquery` options"
    )]
    cquery_command_options: Vec<String>,

    #[arg(long = "fineGrainedHashExternalRepos", value_delimiter = ',')]
    fine_grained_hash_external_repos: Vec<String>,

    #[arg(long = "fineGrainedHashExternalReposFile")]
    fine_grained_hash_external_repos_file: Option<PathBuf>,

    #[arg(
        long = "labelOnlyModuleExtensions",
        value_delimiter = ',',
        help = "Module extensions (`@rules_rs//rs:extensions.bzl%crate`) whose repositories use each \
                other only through labels, never by resolving a toolchain from one another. A \
                fine-grained target in one of their repositories then does not change whenever any \
                other repository of the extension does"
    )]
    label_only_module_extensions: Vec<String>,

    #[arg(
        long = "useCquery",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    use_cquery: bool,

    #[arg(long = "cqueryExpression")]
    cquery_expression: Option<String>,

    #[arg(
        short = 'k',
        long = "keep_going",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    keep_going: bool,

    #[arg(long = "ignoredRuleHashingAttributes", value_delimiter = ',')]
    ignored_rule_hashing_attributes: Vec<String>,

    #[arg(
        long = "excludeExternalTargets",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    exclude_external_targets: bool,

    #[arg(long = "excludeTargetsQuery")]
    exclude_targets_query: Option<String>,

    #[arg(long = "alwaysAffectedTags", value_delimiter = ',')]
    always_affected_tags: Vec<String>,
}

#[derive(Debug, Args)]
struct GenerateHashesArgs {
    #[command(flatten)]
    hashing: HashingArgs,

    #[arg(long = "contentHashPath")]
    content_hash_path: Option<PathBuf>,

    #[arg(
        long = "includeTargetType",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    include_target_type: bool,

    #[arg(long = "targetType", value_delimiter = ',')]
    target_type: Vec<String>,

    #[arg(short = 'd', long = "depEdgesFile")]
    dep_edges_file: Option<PathBuf>,

    #[arg(short = 'm', long = "modified-filepaths")]
    modified_filepaths: Option<PathBuf>,

    /// Opt-in: hash the workspace's git tree (the working tree as it is, committed or not) and
    /// record every rule's hashing recipe, so a later run can derive a revision's hashes from
    /// this file with --baseHashes and no `bazel query` when the diff between the two only edits
    /// regular source files. The file always includes target types. Incompatible with
    /// --useCquery, --fineGrainedHashExternalRepos, --alwaysAffectedTags, --contentHashPath,
    /// --modified-filepaths and --targetType
    #[arg(
        long = "partialGraph",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    partial_graph: bool,

    /// Glob (`*` within a path segment, `**` across, a pattern without `/` matches the file
    /// name) of workspace paths whose change always forces a full generation under --partialGraph
    #[arg(long = "partialGraphRebuildPattern", value_delimiter = ',')]
    partial_graph_rebuild_patterns: Vec<String>,

    /// A hash file written with --partialGraph to derive this revision's hashes from, when the
    /// diff between its tree and the workspace's allows it; otherwise a full generation runs
    #[arg(long = "baseHashes")]
    base_hashes: Option<PathBuf>,

    /// Path to the git executable --partialGraph reads the workspace's tree with
    #[arg(long = "gitPath", default_value = "git")]
    git_path: PathBuf,

    output_path: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct GetImpactedTargetsArgs {
    #[arg(long = "startingHashes")]
    starting_hashes: PathBuf,

    #[arg(long = "finalHashes")]
    final_hashes: PathBuf,

    #[arg(short = 'd', long = "depEdgesFile")]
    dep_edges_file: Option<PathBuf>,

    #[arg(long = "targetType", value_delimiter = ',')]
    target_type: Vec<String>,

    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,

    #[arg(short = 'w', long = "workspacePath", value_parser = parse_normalized_path)]
    workspace_path: PathBuf,

    #[arg(short = 'b', long = "bazelPath", default_value = "bazel")]
    bazel_path: PathBuf,

    #[arg(long = "bazelStartupOptions", allow_hyphen_values = true)]
    bazel_startup_options: Vec<String>,

    #[arg(
        long = "noBazelrc",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    no_bazelrc: bool,

    #[arg(
        long = "excludeExternalTargets",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    exclude_external_targets: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ExplainFormatArg {
    Text,
    Json,
    /// A Graphviz node-link graph of the blame subgraph, edges from root cause to queried target.
    Dot,
    /// A Mermaid `flowchart TD` of the same graph.
    Mermaid,
}

impl From<ExplainFormatArg> for ExplainFormat {
    fn from(value: ExplainFormatArg) -> Self {
        match value {
            ExplainFormatArg::Text => ExplainFormat::Text,
            ExplainFormatArg::Json => ExplainFormat::Json,
            ExplainFormatArg::Dot => ExplainFormat::Dot,
            ExplainFormatArg::Mermaid => ExplainFormat::Mermaid,
        }
    }
}

#[derive(Debug, Args)]
struct ExplainArgs {
    /// The JSON file of target hashes for the initial revision, from `generate-hashes`.
    #[arg(long = "startingHashes")]
    starting_hashes: PathBuf,

    /// The JSON file of target hashes for the final revision, from `generate-hashes`.
    #[arg(long = "finalHashes")]
    final_hashes: PathBuf,

    /// The dependency-edges file written by `generate-hashes --depEdgesFile`. Required:
    /// attribution is a walk over these edges.
    #[arg(short = 'd', long = "depEdgesFile")]
    dep_edges_file: PathBuf,

    /// The impacted Bazel label to explain, e.g. '//service/a:app'.
    #[arg(short = 't', long = "target")]
    target: String,

    /// Output format.
    #[arg(short = 'f', long = "format", value_enum, ignore_case = true, default_value_t = ExplainFormatArg::Text)]
    format: ExplainFormatArg,

    /// Filepath to write the explanation to. Defaults to STDOUT.
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,

    /// Report at most this many root causes, nearest first. The full count is always
    /// reported alongside. 0 means no limit.
    #[arg(long = "maxRootCauses", default_value_t = 25)]
    max_root_causes: usize,

    /// Stop searching this many dependency hops above the queried target. Root causes
    /// further upstream are then not reported (a warning is printed). -1 means no bound.
    #[arg(long = "maxDepth", default_value_t = -1, allow_hyphen_values = true)]
    max_depth: i64,
}

#[derive(Debug, Args)]
struct FingerprintArgs {
    #[command(flatten)]
    hashing: HashingArgs,

    #[arg(
        long = "includeTargetType",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    include_target_type: bool,

    #[arg(long = "targetType", value_delimiter = ',')]
    target_type: Vec<String>,

    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct WarmupArgs {
    #[command(flatten)]
    generate: GenerateHashesArgs,

    #[arg(long = "base-hashes", default_value = "/snap/base_hashes.json")]
    base_hashes: PathBuf,

    #[arg(long = "fingerprint-output", default_value = "/snap/fingerprint.json")]
    fingerprint_output: PathBuf,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    hashing: HashingArgs,

    #[arg(long = "gitPath", default_value = "git")]
    git_path: PathBuf,

    #[arg(long, default_value_t = 8080)]
    port: u16,

    #[arg(long = "requestTimeout", default_value_t = 0)]
    request_timeout: u64,

    #[arg(long = "cacheDir", value_parser = parse_normalized_path)]
    cache_dir: PathBuf,

    #[arg(
        long = "trackDeps",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    track_deps: bool,

    /// Opt-in: only serve cached entries whose external-dependency fingerprint still matches
    #[arg(
        long = "dependencyFingerprint",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    dependency_fingerprint: bool,

    /// Opt-in: derive a revision's hashes from the cached `from` entry without a `bazel query`
    /// when the diff between them only edits regular source files; every entry then also records
    /// the hashing recipes that makes possible. Incompatible with --useCquery,
    /// --fineGrainedHashExternalRepos and --alwaysAffectedTags
    #[arg(
        long = "partialGraph",
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = false
    )]
    partial_graph: bool,

    /// Glob (`*` within a path segment, `**` across, a pattern without `/` matches the file
    /// name) of workspace paths whose change always forces a full generation under --partialGraph
    #[arg(long = "partialGraphRebuildPattern", value_delimiter = ',')]
    partial_graph_rebuild_patterns: Vec<String>,

    /// Cache a revision whose bazel query loaded packages its git tree does not contain
    /// (untracked or ignored BUILD files in the working tree) instead of failing the request
    #[arg(long = "allowUntrackedPackages")]
    allow_untracked_packages: bool,

    #[arg(long = "no-initial-fetch")]
    no_initial_fetch: bool,

    #[arg(long = "warmupRevision", value_delimiter = ',')]
    warmup_revisions: Vec<String>,

    #[arg(long = "cacheMaxAge")]
    cache_max_age: Option<String>,

    #[arg(long = "cacheMaxEntries")]
    cache_max_entries: Option<usize>,

    #[arg(long = "cacheMaxSize")]
    cache_max_size: Option<String>,

    #[arg(long = "cachePruneInterval", default_value = "1h")]
    cache_prune_interval: String,

    #[arg(long = "cacheMemoryMax", default_value = "512MB")]
    cache_memory_max: String,

    #[arg(long = "s3Bucket", conflicts_with = "nexus_url")]
    s3_bucket: Option<String>,

    #[arg(long = "s3Prefix", default_value = "")]
    s3_prefix: String,

    #[arg(long = "s3Region")]
    s3_region: Option<String>,

    #[arg(long = "s3Endpoint")]
    s3_endpoint: Option<String>,

    #[arg(long = "s3ForcePathStyle")]
    s3_force_path_style: bool,

    /// Base URL of a Sonatype Nexus Repository instance used as the shared cache tier
    #[arg(long = "nexusUrl", requires = "nexus_repository")]
    nexus_url: Option<String>,

    /// Raw hosted repository on --nexusUrl that holds cache entries
    #[arg(long = "nexusRepository", requires = "nexus_url")]
    nexus_repository: Option<String>,

    #[arg(long = "nexusPrefix", default_value = "")]
    nexus_prefix: String,

    /// Basic-auth user; the password is read from the BAZEL_DIFF_NEXUS_PASSWORD environment variable
    #[arg(long = "nexusUsername", env = "BAZEL_DIFF_NEXUS_USERNAME")]
    nexus_username: Option<String>,

    /// PEM bundle of extra CA certificates to trust for --nexusUrl
    #[arg(long = "nexusCaCert", value_parser = parse_normalized_path)]
    nexus_ca_cert: Option<PathBuf>,
}

/// The Nexus password is environment-only so it never shows up in a process listing.
const NEXUS_PASSWORD_ENV: &str = "BAZEL_DIFF_NEXUS_PASSWORD";

fn flatten_options(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(' '))
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Folds `.` and `..` away lexically. Only meaningful on an absolute path:
/// applied to a relative one, `.` and a leading `..` have nothing to resolve
/// against and simply vanish, so callers go through [`parse_normalized_path`].
fn normalize_path(path: PathBuf) -> PathBuf {
    path.components()
        .fold(PathBuf::new(), |mut normalized, component| {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                other => normalized.push(other.as_os_str()),
            }
            normalized
        })
}

/// Resolves a path argument against the process working directory, then
/// normalizes it.
///
/// Making the path absolute *first* is what keeps a relative argument
/// meaningful. Normalizing `.` on its own drops its only component and leaves
/// the empty path, which then fails every `Command::current_dir` with a
/// `chdir("")` ENOENT that names the Bazel binary rather than the workspace;
/// normalizing `../sibling` on its own pops nothing and silently yields
/// `sibling`. Anchoring to the working directory gives both the directory the
/// user meant. See
/// <https://github.com/Tinder/bazel-diff/issues/470>.
fn parse_normalized_path(value: &str) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(value)
        .map_err(|error| format!("failed to resolve path {value:?}: {error}"))?;
    Ok(normalize_path(absolute))
}

fn read_lines(path: Option<&Path>) -> Result<BTreeSet<PathBuf>> {
    let Some(path) = path else {
        return Ok(BTreeSet::new());
    };
    Ok(fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn read_string_lines(path: &Path) -> Result<BTreeSet<String>> {
    Ok(fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn bazel_options(args: &HashingArgs, verbose: bool) -> Result<BazelOptions> {
    if !args.fine_grained_hash_external_repos.is_empty()
        && args.fine_grained_hash_external_repos_file.is_some()
    {
        bail!(
            "--fineGrainedHashExternalRepos and --fineGrainedHashExternalReposFile are mutually exclusive"
        );
    }
    let mut repos = args
        .fine_grained_hash_external_repos
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if let Some(path) = &args.fine_grained_hash_external_repos_file {
        repos.extend(read_string_lines(path)?);
    }
    Ok(BazelOptions {
        workspace: args.workspace_path.clone(),
        bazel: args.bazel_path.clone(),
        startup_options: flatten_options(&args.bazel_startup_options),
        command_options: flatten_options(&args.bazel_command_options),
        cquery_options: flatten_options(&args.cquery_command_options),
        use_cquery: args.use_cquery,
        cquery_expression: args.cquery_expression.clone(),
        keep_going: args.keep_going,
        fine_grained_external_repos: repos,
        label_only_module_extensions: args.label_only_module_extensions.iter().cloned().collect(),
        exclude_external_targets: args.exclude_external_targets,
        exclude_targets_query: args.exclude_targets_query.clone(),
        no_bazelrc: false,
        verbose,
        cancellation: None,
    })
}

fn hash_options(
    args: &HashingArgs,
    content_hash_path: Option<&Path>,
    modified_filepaths: Option<&Path>,
    track_deps: bool,
    verbose: bool,
) -> Result<HashOptions> {
    let content_hashes = content_hash_path
        .map(fs::read)
        .transpose()
        .context("failed to read content hash file")?
        .map(|bytes| serde_json::from_slice::<BTreeMap<String, String>>(&bytes))
        .transpose()
        .context("invalid content hash JSON")?;
    Ok(HashOptions {
        bazel: bazel_options(args, verbose)?,
        content_hashes,
        ignored_attributes: args
            .ignored_rule_hashing_attributes
            .iter()
            .cloned()
            .collect(),
        seed_filepaths: read_lines(args.seed_filepaths.as_deref())?,
        modified_filepaths: read_lines(modified_filepaths)?,
        track_deps,
        always_affected_tags: args.always_affected_tags.iter().cloned().collect(),
        partial_tree: None,
    })
}

fn fingerprint_flags(
    args: &HashingArgs,
    include_type: bool,
    types: &[String],
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "bazelStartupOptions".into(),
            flatten_options(&args.bazel_startup_options).join(" "),
        ),
        (
            "bazelCommandOptions".into(),
            flatten_options(&args.bazel_command_options).join(" "),
        ),
        (
            "cqueryCommandOptions".into(),
            flatten_options(&args.cquery_command_options).join(" "),
        ),
        ("useCquery".into(), args.use_cquery.to_string()),
        (
            "cqueryExpression".into(),
            args.cquery_expression.clone().unwrap_or_default(),
        ),
        ("includeTargetType".into(), include_type.to_string()),
        (
            "targetType".into(),
            types
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "fineGrainedHashExternalRepos".into(),
            args.fine_grained_hash_external_repos
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "labelOnlyModuleExtensions".into(),
            args.label_only_module_extensions
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "ignoredRuleHashingAttributes".into(),
            args.ignored_rule_hashing_attributes
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "excludeExternalTargets".into(),
            args.exclude_external_targets.to_string(),
        ),
        ("keepGoing".into(), args.keep_going.to_string()),
    ])
}

fn write_json_file(file: File, value: &impl Serialize) -> Result<()> {
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, value)?;
    writer.flush()?;
    Ok(())
}

fn write_json(path: Option<&Path>, value: &impl Serialize) -> Result<()> {
    if let Some(path) = path.filter(|path| *path != Path::new("-")) {
        let file =
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        return write_json_file(file, value)
            .with_context(|| format!("failed to write {}", path.display()));
    }

    let mut writer = BufWriter::new(io::stdout().lock());
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn run_generate(args: &GenerateHashesArgs, verbose: bool) -> Result<HashFileData> {
    run_generate_with(args, verbose, generate_hashes)
}

fn run_generate_with(
    args: &GenerateHashesArgs,
    verbose: bool,
    generate: impl FnOnce(&HashOptions) -> Result<HashFileData>,
) -> Result<HashFileData> {
    let mut options = hash_options(
        &args.hashing,
        args.content_hash_path.as_deref(),
        args.modified_filepaths.as_deref(),
        args.dep_edges_file.is_some(),
        verbose,
    )?;
    let mut data = if args.partial_graph {
        partial_graph_hashes(args, &mut options, generate)?
    } else if args.base_hashes.is_some() {
        bail!("--baseHashes needs --partialGraph");
    } else {
        generate(&options)?
    };
    if !args.target_type.is_empty() {
        let types = args.target_type.iter().cloned().collect::<HashSet<_>>();
        data.hashes.retain(|_, hash| types.contains(&hash.kind));
    }
    // A partial-graph file carries every target's type and its dependency edges, which a
    // derivation from it needs to evaluate the recipes and to write --depEdgesFile.
    write_json(
        args.output_path.as_deref(),
        &data.serialized(
            args.include_target_type || args.partial_graph,
            args.partial_graph && args.dep_edges_file.is_some(),
        ),
    )?;
    if let Some(path) = &args.dep_edges_file {
        let context = || format!("failed to write {}", path.display());
        let file = File::create(path).with_context(context)?;
        write_json_file(file, &data.dep_edges).with_context(context)?;
    }
    Ok(data)
}

/// `--partialGraph` for `generate-hashes`: the hashes describe the working tree's git tree, and
/// come from re-evaluating the `--baseHashes` entry's recipes against it when the diff between
/// the two allows it (see [`bazel_diff::partial`]), or from a full generation that records the
/// recipes otherwise. Either way the file written is the same.
fn partial_graph_hashes(
    args: &GenerateHashesArgs,
    options: &mut HashOptions,
    generate: impl FnOnce(&HashOptions) -> Result<HashFileData>,
) -> Result<HashFileData> {
    partial::validate_hash_options(options)?;
    if args.content_hash_path.is_some() {
        bail!("--partialGraph does not support --contentHashPath: source content is read from the git tree");
    }
    if args.modified_filepaths.is_some() {
        bail!("--partialGraph does not support --modified-filepaths");
    }
    if !args.target_type.is_empty() {
        bail!("--partialGraph does not support --targetType: a file missing targets cannot be derived from (filter in get-impacted-targets instead)");
    }
    let rebuild_patterns = args
        .partial_graph_rebuild_patterns
        .iter()
        .map(|pattern| Glob::new(pattern))
        .collect::<Result<Vec<_>>>()?;
    let git = Git::new(&args.git_path, &args.hashing.workspace_path);
    // bazel-diff's own files are not part of the revision, wherever the user keeps them.
    let own_files = [
        args.output_path.as_deref(),
        args.base_hashes.as_deref(),
        args.dep_edges_file.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let tree = git.snapshot_working_tree(&own_files)?;
    let tree_id = tree
        .id()
        .expect("a working tree snapshot carries its tree id")
        .to_owned();
    eprintln!(
        "[Info] partial graph: workspace tree {tree_id} ({} tracked paths)",
        tree.len()
    );
    let fallback = match &args.base_hashes {
        Some(path) => {
            let base = HashFileData::read(path)
                .with_context(|| format!("failed to read --baseHashes {}", path.display()))?;
            match base.tree.as_deref() {
                _ if base.partial.is_none() => {
                    format!("{} carries no partial-graph recipes", path.display())
                }
                _ if args.dep_edges_file.is_some()
                    && base.dep_edges.is_empty()
                    && !base.hashes.is_empty() =>
                {
                    format!(
                        "{} records no dependency edges, which --depEdgesFile needs",
                        path.display()
                    )
                }
                None => format!("{} records no git tree", path.display()),
                Some(base_tree) if base_tree == tree_id => {
                    eprintln!(
                        "[Info] partial graph: {} already describes tree {tree_id}",
                        path.display()
                    );
                    return Ok(base);
                }
                Some(base_tree) => {
                    let policy = DerivationPolicy {
                        seed_filepaths: &options.seed_filepaths,
                        rebuild_patterns: &rebuild_patterns,
                    };
                    match git::classify_revisions(&git, base_tree, &tree_id, &base, &tree, &policy)
                    {
                        Ok(Classification::Derivable {
                            changed_sources,
                            unmatched_paths,
                        }) => match partial::derive(&base, &tree) {
                            Ok(data) => {
                                eprintln!(
                                    "[Info] partial graph: derived {tree_id} from {base_tree}: {} changed source targets, {} changed paths outside the graph, {} targets",
                                    changed_sources.len(),
                                    unmatched_paths.len(),
                                    data.hashes.len()
                                );
                                return Ok(data);
                            }
                            Err(error) => format!("derivation failed: {error:#}"),
                        },
                        Ok(Classification::Rebuild { reason }) => reason,
                        Err(error) => format!("classification failed: {error:#}"),
                    }
                }
            }
        }
        None => "no --baseHashes to derive from".to_owned(),
    };
    eprintln!("[Info] partial graph: full generation for {tree_id}: {fallback}");
    options.content_hashes = Some(tree.content_hashes());
    options.partial_tree = Some(Arc::new(tree));
    generate(options)
}

/// Hashes of the two modes follow different formulas, so comparing a `--partialGraph` file with
/// a default-mode one would report every target as impacted.
fn ensure_same_mode(from: &HashFileData, to: &HashFileData) -> Result<()> {
    if from.partial.is_some() != to.partial.is_some() {
        bail!(
            "the starting and final hashes were not generated in the same mode: one carries --partialGraph recipes and the other does not, and hashes of the two modes are not comparable"
        );
    }
    Ok(())
}

fn get_bazel_options(args: &GetImpactedTargetsArgs, verbose: bool) -> BazelOptions {
    BazelOptions {
        workspace: args.workspace_path.clone(),
        bazel: args.bazel_path.clone(),
        startup_options: flatten_options(&args.bazel_startup_options),
        command_options: Vec::new(),
        cquery_options: Vec::new(),
        use_cquery: false,
        cquery_expression: None,
        keep_going: false,
        fine_grained_external_repos: BTreeSet::new(),
        label_only_module_extensions: BTreeSet::new(),
        exclude_external_targets: false,
        exclude_targets_query: None,
        no_bazelrc: args.no_bazelrc,
        verbose,
        cancellation: None,
    }
}

fn run_get_impacted(args: &GetImpactedTargetsArgs, verbose: bool) -> Result<()> {
    let from = HashFileData::read(&args.starting_hashes)?;
    let to = HashFileData::read(&args.final_hashes)?;
    ensure_same_mode(&from, &to)?;
    let bazel = get_bazel_options(args, verbose);
    let exclude_external = args
        .exclude_external_targets
        .unwrap_or_else(|| bazel.is_bzlmod_enabled());
    let target_types = (!args.target_type.is_empty())
        .then(|| args.target_type.iter().cloned().collect::<HashSet<_>>());
    let output = if let Some(dep_edges_file) = &args.dep_edges_file {
        let dep_edges: BTreeMap<String, Vec<String>> =
            serde_json::from_slice(&fs::read(dep_edges_file)?)?;
        let module_impacted = impacted_with_module_changes(&from, &to, Some(&bazel))?;
        let hash_impacted = impacted_targets(&from.hashes, &to.hashes, None, false)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut distance_from = from.hashes.clone();
        for label in module_impacted.difference(&hash_impacted) {
            distance_from.remove(label);
        }
        serde_json::to_string(&impacted_targets_with_distances(
            &distance_from,
            &to.hashes,
            &dep_edges,
            target_types.as_ref(),
            exclude_external,
        )?)?
    } else {
        let impacted = impacted_with_module_changes(&from, &to, Some(&bazel))?;
        filter_and_sort_labels(
            impacted,
            &from.hashes,
            &to.hashes,
            target_types.as_ref(),
            exclude_external,
        )?
        .into_iter()
        .map(|label| format!("{label}\n"))
        .collect()
    };
    match &args.output {
        Some(path) => fs::write(path, output)?,
        None => print!("{output}"),
    }
    Ok(())
}

fn run_explain(args: &ExplainArgs, verbose: bool) -> Result<()> {
    let from = HashFileData::read(&args.starting_hashes)
        .context("Incorrect starting hashes: file doesn't exist or can't be read.")?;
    let to = HashFileData::read(&args.final_hashes)
        .context("Incorrect final hashes: file doesn't exist or can't be read.")?;
    ensure_same_mode(&from, &to)?;
    let dep_edges: BTreeMap<String, Vec<String>> = serde_json::from_slice(
        &fs::read(&args.dep_edges_file)
            .context("Incorrect dep edges file: file doesn't exist or can't be read.")?,
    )
    .context("Incorrect dep edges file: expected a JSON object mapping labels to label arrays")?;
    let limits = ExplainLimits {
        max_depth: usize::try_from(args.max_depth).ok(),
        max_roots: args.max_root_causes,
    };
    let outcome = explain::explain(&from.hashes, &to.hashes, &dep_edges, &args.target, limits)?;
    for warning in &outcome.warnings {
        eprintln!("[Warning] {warning}");
    }
    if verbose {
        for note in &outcome.notes {
            eprintln!("[Info] {note}");
        }
    }
    let rendered = explain::render(&outcome.explanation, args.format.into());
    match &args.output {
        Some(path) => fs::write(path, rendered)
            .with_context(|| format!("failed to write {}", path.display()))?,
        None => print!("{rendered}"),
    }
    Ok(())
}

fn run_fingerprint(args: &FingerprintArgs) -> Result<()> {
    let flags = fingerprint_flags(&args.hashing, args.include_target_type, &args.target_type);
    let inputs = fingerprint::gather(
        &args.hashing.workspace_path,
        &args.hashing.bazel_path,
        flags.clone(),
    );
    fingerprint::write_json(
        args.output.as_deref(),
        &fingerprint::compute(&inputs),
        &flags,
    )
}

fn run_warmup(args: &WarmupArgs, verbose: bool) -> Result<()> {
    run_warmup_with(
        args,
        |generate| run_generate(generate, verbose).map(|_| ()),
        run_fingerprint,
    )
}

fn run_warmup_with(
    args: &WarmupArgs,
    generate_hashes: impl FnOnce(&GenerateHashesArgs) -> Result<()>,
    write_fingerprint: impl FnOnce(&FingerprintArgs) -> Result<()>,
) -> Result<()> {
    if let Some(parent) = args.base_hashes.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut generate = GenerateHashesArgs {
        hashing: args.generate.hashing.clone(),
        content_hash_path: args.generate.content_hash_path.clone(),
        include_target_type: args.generate.include_target_type,
        target_type: args.generate.target_type.clone(),
        dep_edges_file: args.generate.dep_edges_file.clone(),
        modified_filepaths: args.generate.modified_filepaths.clone(),
        partial_graph: args.generate.partial_graph,
        partial_graph_rebuild_patterns: args.generate.partial_graph_rebuild_patterns.clone(),
        base_hashes: args.generate.base_hashes.clone(),
        git_path: args.generate.git_path.clone(),
        output_path: Some(args.base_hashes.clone()),
    };
    generate.output_path = Some(args.base_hashes.clone());
    generate_hashes(&generate)?;
    if let Some(parent) = args.fingerprint_output.parent() {
        fs::create_dir_all(parent)?;
    }
    let fingerprint_args = FingerprintArgs {
        hashing: args.generate.hashing.clone(),
        include_target_type: args.generate.include_target_type,
        target_type: args.generate.target_type.clone(),
        output: Some(args.fingerprint_output.clone()),
    };
    write_fingerprint(&fingerprint_args)
}

impl ServeArgs {
    fn to_config(&self, verbose: bool) -> Result<bazel_diff::server::ServerConfig> {
        self.to_config_with_nexus_password(verbose, std::env::var(NEXUS_PASSWORD_ENV).ok())
    }

    fn to_config_with_nexus_password(
        &self,
        verbose: bool,
        nexus_password: Option<String>,
    ) -> Result<bazel_diff::server::ServerConfig> {
        if self.s3_bucket.is_some() && self.s3_endpoint.as_deref().is_some_and(str::is_empty) {
            bail!("--s3Endpoint must not be empty");
        }
        let nexus_credentials = match (&self.nexus_username, nexus_password) {
            _ if self.nexus_url.is_none() => None,
            (Some(username), Some(password)) => Some(bazel_diff::server::NexusCredentials {
                username: username.clone(),
                password,
            }),
            (None, None) => None,
            (Some(_), None) => bail!("--nexusUsername requires {NEXUS_PASSWORD_ENV} to be set"),
            (None, Some(_)) => bail!("{NEXUS_PASSWORD_ENV} requires --nexusUsername"),
        };
        let remote_cache = match (&self.s3_bucket, &self.nexus_url, &self.nexus_repository) {
            (Some(bucket), _, _) => {
                let prefix = self.s3_prefix.trim_matches('/');
                Some(if prefix.is_empty() {
                    format!("s3://{bucket}/")
                } else {
                    format!("s3://{bucket}/{prefix}/")
                })
            }
            (None, Some(url), Some(repository)) => Some(
                bazel_diff::server::nexus_base_url(url, repository, &self.nexus_prefix)?
                    .to_string(),
            ),
            _ => None,
        };
        let config = bazel_diff::server::ServerConfig {
            hash_options: hash_options(&self.hashing, None, None, self.track_deps, verbose)?,
            git_path: self.git_path.clone(),
            port: self.port,
            request_timeout: Duration::from_secs(self.request_timeout),
            cache_dir: self.cache_dir.clone(),
            track_deps: self.track_deps,
            dependency_fingerprint: self.dependency_fingerprint,
            partial_graph: self.partial_graph,
            partial_graph_rebuild_patterns: self
                .partial_graph_rebuild_patterns
                .iter()
                .map(|pattern| bazel_diff::partial::Glob::new(pattern))
                .collect::<Result<Vec<_>>>()?,
            allow_untracked_packages: self.allow_untracked_packages,
            no_initial_fetch: self.no_initial_fetch,
            warmup_revisions: self.warmup_revisions.clone(),
            cache_max_age: self
                .cache_max_age
                .as_deref()
                .map(bazel_diff::server::parse_duration)
                .transpose()?,
            cache_max_entries: self.cache_max_entries,
            cache_max_size: self
                .cache_max_size
                .as_deref()
                .map(bazel_diff::server::parse_byte_size)
                .transpose()?,
            cache_prune_interval: bazel_diff::server::parse_duration(&self.cache_prune_interval)?,
            cache_memory_max: bazel_diff::server::parse_byte_size(&self.cache_memory_max)?,
            remote_cache,
            s3_bucket: self.s3_bucket.clone(),
            s3_prefix: self.s3_prefix.clone(),
            s3_region: self.s3_region.clone(),
            s3_endpoint: self.s3_endpoint.clone(),
            s3_force_path_style: self.s3_force_path_style,
            nexus_url: self.nexus_url.clone(),
            nexus_repository: self.nexus_repository.clone(),
            nexus_prefix: self.nexus_prefix.clone(),
            nexus_credentials,
            nexus_ca_cert: self.nexus_ca_cert.clone(),
        };
        bazel_diff::server::validate_partial_graph(&config)?;
        Ok(config)
    }
}

fn normalized_argv() -> Vec<String> {
    std::env::args().map(normalize_argument).collect()
}

fn normalize_argument(argument: String) -> String {
    match argument.as_str() {
        "-so" => "--bazelStartupOptions".to_owned(),
        "-co" => "--bazelCommandOptions".to_owned(),
        "-tt" => "--targetType".to_owned(),
        "-sh" => "--startingHashes".to_owned(),
        "-fh" => "--finalHashes".to_owned(),
        "--no-useCquery" => "--useCquery=false".to_owned(),
        "--no-keep_going" => "--keep_going=false".to_owned(),
        "--no-includeTargetType" => "--includeTargetType=false".to_owned(),
        "--no-excludeExternalTargets" => "--excludeExternalTargets=false".to_owned(),
        "--no-noBazelrc" => "--noBazelrc=false".to_owned(),
        "--no-trackDeps" => "--trackDeps=false".to_owned(),
        "--no-dependencyFingerprint" => "--dependencyFingerprint=false".to_owned(),
        _ => argument,
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("[Error] {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse_from(normalized_argv());
    match &cli.command {
        Commands::GenerateHashes(args) => {
            run_generate(args, cli.verbose)?;
        }
        Commands::GetImpactedTargets(args) => run_get_impacted(args, cli.verbose)?,
        Commands::Explain(args) => run_explain(args, cli.verbose)?,
        Commands::Fingerprint(args) => run_fingerprint(args)?,
        Commands::Warmup(args) => run_warmup(args, cli.verbose)?,
        Commands::Serve(args) => {
            bazel_diff::server::serve(args.to_config(cli.verbose)?)?;
        }
    }
    io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn parse(arguments: &[&str]) -> Cli {
        Cli::try_parse_from(
            arguments
                .iter()
                .map(|argument| normalize_argument((*argument).to_owned())),
        )
        .unwrap()
    }

    fn hashing_args(workspace: &Path) -> HashingArgs {
        HashingArgs {
            workspace_path: workspace.to_path_buf(),
            bazel_path: PathBuf::from("bazel"),
            seed_filepaths: None,
            bazel_startup_options: Vec::new(),
            bazel_command_options: Vec::new(),
            cquery_command_options: Vec::new(),
            fine_grained_hash_external_repos: Vec::new(),
            fine_grained_hash_external_repos_file: None,
            label_only_module_extensions: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            keep_going: false,
            ignored_rule_hashing_attributes: Vec::new(),
            exclude_external_targets: false,
            exclude_targets_query: None,
            always_affected_tags: Vec::new(),
        }
    }

    #[test]
    fn normalizes_legacy_short_flags() {
        let arguments = [
            "bazel-diff",
            "get-impacted-targets",
            "-sh",
            "from.json",
            "-fh",
            "to.json",
            "-w",
            ".",
            "-so",
            "--batch",
            "-tt",
            "Rule",
        ];
        let normalized = arguments
            .into_iter()
            .map(str::to_owned)
            .map(normalize_argument)
            .collect::<Vec<_>>();

        let cli = Cli::try_parse_from(normalized).unwrap();
        let Commands::GetImpactedTargets(args) = cli.command else {
            panic!("expected get-impacted-targets");
        };
        assert_eq!(args.starting_hashes, PathBuf::from("from.json"));
        assert_eq!(args.final_hashes, PathBuf::from("to.json"));
        assert_eq!(args.bazel_startup_options, ["--batch"]);
        assert_eq!(args.target_type, ["Rule"]);
    }

    #[test]
    fn parses_picocli_negated_booleans() {
        let arguments = [
            "bazel-diff",
            "generate-hashes",
            "-w",
            ".",
            "--no-useCquery",
            "--no-keep_going",
            "--no-includeTargetType",
            "--no-excludeExternalTargets",
            "hashes.json",
        ];
        let normalized = arguments
            .into_iter()
            .map(str::to_owned)
            .map(normalize_argument)
            .collect::<Vec<_>>();

        let cli = Cli::try_parse_from(normalized).unwrap();
        let Commands::GenerateHashes(args) = cli.command else {
            panic!("expected generate-hashes");
        };
        assert!(!args.hashing.use_cquery);
        assert!(!args.hashing.keep_going);
        assert!(!args.include_target_type);
        assert!(!args.hashing.exclude_external_targets);
    }

    #[test]
    fn option_and_path_compatibility_helpers_normalize_values() {
        assert_eq!(
            flatten_options(&["a b".to_owned(), " c  d ".to_owned()]),
            ["a", "b", "c", "d"]
        );
        assert_eq!(
            normalize_path(PathBuf::from("/home/../some-dir")),
            PathBuf::from("/some-dir")
        );
    }

    /// Regression test for <https://github.com/Tinder/bazel-diff/issues/470>.
    ///
    /// `--workspacePath` and `--cacheDir` are anchored to the process working
    /// directory before they are normalized. Without that, a path built only
    /// out of `.`/`..` components collapsed to the empty path, and a leading
    /// `..` was silently dropped.
    #[test]
    fn relative_path_arguments_resolve_against_the_working_directory() {
        let working_directory = std::env::current_dir().unwrap();
        for value in [".", "./", "./nested/..", "nested/.."] {
            assert_eq!(
                parse_normalized_path(value).unwrap(),
                working_directory,
                "--workspacePath {value} should be the working directory"
            );
        }
        assert_eq!(
            parse_normalized_path("nested").unwrap(),
            working_directory.join("nested")
        );
        assert_eq!(
            parse_normalized_path("./nested/inner/..").unwrap(),
            working_directory.join("nested")
        );

        // A leading `..` has a parent to pop now that the path is absolute.
        assert_eq!(
            parse_normalized_path("../sibling").unwrap(),
            working_directory.parent().unwrap().join("sibling")
        );

        // Absolute arguments keep the normalization they always had.
        let absolute = working_directory.join("nested/inner/..");
        assert_eq!(
            parse_normalized_path(absolute.to_str().unwrap()).unwrap(),
            working_directory.join("nested")
        );

        // The empty path never named a directory; it is now rejected instead of
        // being passed to `chdir`.
        assert!(parse_normalized_path("").is_err());
    }

    /// Regression test for <https://github.com/Tinder/bazel-diff/issues/470>:
    /// the symptom users hit.
    ///
    /// [`BazelOptions::command`] does `command.current_dir(&self.workspace)`.
    /// When `-w .` normalized to the empty path the child's `chdir("")` failed,
    /// and `std::process::Command` surfaced that as a spawn failure -- so the
    /// error named the *Bazel binary*, which exists and is executable, instead
    /// of the workspace:
    ///
    /// ```text
    /// [Error] failed to execute /path/to/bazel: No such file or directory (os error 2)
    /// ```
    #[cfg(unix)]
    #[test]
    fn relative_workspace_path_spawns_bazel_in_the_working_directory() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let directory = tempfile::tempdir().unwrap();
        let stub_bazel = directory.path().join("bazel");
        fs::write(&stub_bazel, "#!/bin/sh\npwd -P\n").unwrap();
        fs::set_permissions(&stub_bazel, fs::Permissions::from_mode(0o755)).unwrap();

        let cli = parse(&[
            "bazel-diff",
            "generate-hashes",
            "-w",
            ".",
            "-b",
            stub_bazel.to_str().unwrap(),
            "hashes.json",
        ]);
        let Commands::GenerateHashes(args) = cli.command else {
            panic!("expected generate-hashes");
        };
        let options = bazel_options(&args.hashing, false).unwrap();

        // Mirrors BazelOptions::command().
        let output = Command::new(&options.bazel)
            .current_dir(&options.workspace)
            .arg("version")
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "`-w .` gave workspace {:?}, so spawning {} failed: {error}",
                    options.workspace,
                    options.bazel.display()
                )
            });

        assert!(output.status.success());
        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()),
            std::env::current_dir().unwrap(),
            "the child should run in the working directory `.` named"
        );
    }

    #[test]
    fn reads_and_trims_seed_and_repository_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("values.txt");
        fs::write(&path, "a/b.txt\n\n  \nc/d.txt\n a/b.txt \n").unwrap();
        assert_eq!(
            read_lines(Some(&path)).unwrap(),
            BTreeSet::from([PathBuf::from("a/b.txt"), PathBuf::from("c/d.txt")])
        );
        assert_eq!(
            read_string_lines(&path).unwrap(),
            BTreeSet::from(["a/b.txt".to_owned(), "c/d.txt".to_owned()])
        );
        assert!(read_lines(None).unwrap().is_empty());
        assert!(read_lines(Some(Path::new("/missing/seed-file"))).is_err());
    }

    #[test]
    fn bazel_options_reject_conflicting_external_repo_sources() {
        let directory = tempfile::tempdir().unwrap();
        let repos = directory.path().join("repos.txt");
        fs::write(&repos, "@from-file\n").unwrap();
        let mut args = hashing_args(directory.path());
        args.fine_grained_hash_external_repos = vec!["@inline".into()];
        args.fine_grained_hash_external_repos_file = Some(repos.clone());
        assert!(bazel_options(&args, false).is_err());

        args.fine_grained_hash_external_repos.clear();
        let options = bazel_options(&args, true).unwrap();
        assert_eq!(
            options.fine_grained_external_repos,
            BTreeSet::from(["@from-file".to_owned()])
        );
        assert!(options.verbose);
    }

    #[test]
    fn hash_options_load_content_hashes_and_validate_shape() {
        let directory = tempfile::tempdir().unwrap();
        let content = directory.path().join("content.json");
        let modified = directory.path().join("modified.txt");
        let seeds = directory.path().join("seeds.txt");
        fs::write(&content, r#"{"pkg/file":"digest"}"#).unwrap();
        fs::write(&modified, "pkg/file\n").unwrap();
        fs::write(&seeds, "seed.txt\n").unwrap();
        let mut args = hashing_args(directory.path());
        args.seed_filepaths = Some(seeds);
        args.ignored_rule_hashing_attributes = vec!["visibility".into()];
        args.always_affected_tags = vec!["external".into()];
        let options = hash_options(&args, Some(&content), Some(&modified), true, false).unwrap();
        assert_eq!(options.content_hashes.unwrap()["pkg/file"], "digest");
        assert!(options.seed_filepaths.contains(Path::new("seed.txt")));
        assert!(options.modified_filepaths.contains(Path::new("pkg/file")));
        assert!(options.ignored_attributes.contains("visibility"));
        assert!(options.always_affected_tags.contains("external"));
        assert!(options.track_deps);

        fs::write(&content, "[]").unwrap();
        assert!(hash_options(&args, Some(&content), None, false, false).is_err());
    }

    #[test]
    fn fingerprint_flags_are_sorted_and_order_independent() {
        let directory = tempfile::tempdir().unwrap();
        let mut first = hashing_args(directory.path());
        first.bazel_startup_options = vec!["--a --b".into()];
        first.bazel_command_options = vec!["--c".into()];
        first.use_cquery = true;
        first.cquery_expression = Some("deps(//...)".into());
        first.keep_going = true;
        first.exclude_external_targets = true;
        first.fine_grained_hash_external_repos = vec!["maven".into(), "abc".into()];
        first.ignored_rule_hashing_attributes = vec!["z".into(), "a".into()];
        first.label_only_module_extensions = vec!["@b//:e.bzl%b".into(), "@a//:e.bzl%a".into()];
        let mut second = first.clone();
        second.fine_grained_hash_external_repos.reverse();
        second.label_only_module_extensions.reverse();
        second.ignored_rule_hashing_attributes.reverse();

        let first_flags = fingerprint_flags(&first, true, &["Rule".into(), "GeneratedFile".into()]);
        let second_flags =
            fingerprint_flags(&second, true, &["GeneratedFile".into(), "Rule".into()]);
        assert_eq!(first_flags, second_flags);
        assert_eq!(first_flags["bazelStartupOptions"], "--a --b");
        assert_eq!(first_flags["targetType"], "GeneratedFile,Rule");
        assert_eq!(first_flags["fineGrainedHashExternalRepos"], "abc,maven");
        assert_eq!(
            first_flags["labelOnlyModuleExtensions"],
            "@a//:e.bzl%a,@b//:e.bzl%b"
        );
    }

    #[test]
    fn writes_json_to_file_and_dash_uses_stdout_path() {
        let output = tempfile::NamedTempFile::new().unwrap();
        write_json(Some(output.path()), &BTreeMap::from([("b", 2), ("a", 1)])).unwrap();
        let mut text = String::new();
        File::open(output.path())
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["a"], 1);
        assert_eq!(value["b"], 2);

        let directory = tempfile::tempdir().unwrap();
        let dash = directory.path().join("-");
        write_json(Some(Path::new("-")), &BTreeMap::from([("a", 1)])).unwrap();
        assert!(!dash.exists());
        assert!(write_json(
            Some(Path::new("/definitely/missing/parent/out.json")),
            &BTreeMap::from([("a", 1)])
        )
        .is_err());
    }

    #[test]
    fn serve_config_parses_pruning_s3_and_tracking_flags() {
        let cli = parse(&[
            "bazel-diff",
            "serve",
            "--workspacePath",
            "/tmp/ws",
            "--cacheDir",
            "/tmp/cache",
            "--trackDeps",
            "--dependencyFingerprint",
            "--requestTimeout",
            "30",
            "--cacheMaxAge",
            "7d",
            "--cacheMaxSize",
            "10gb",
            "--cacheMaxEntries",
            "500",
            "--cacheMemoryMax",
            "2gb",
            "--s3Bucket",
            "bucket",
            "--s3Prefix",
            "/team/repo/",
            "--s3Region",
            "us-east-1",
            "--s3Endpoint",
            "http://localhost:9000",
            "--s3ForcePathStyle",
            "--warmupRevision",
            "main,release",
        ]);
        let Commands::Serve(args) = cli.command else {
            panic!("expected serve");
        };
        let config = args.to_config(true).unwrap();
        assert!(config.track_deps);
        assert!(config.dependency_fingerprint);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.cache_max_age, Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(config.cache_max_size, Some(10 * 1024u64.pow(3)));
        assert_eq!(config.cache_max_entries, Some(500));
        assert_eq!(config.cache_prune_interval, Duration::from_secs(3600));
        assert_eq!(config.cache_memory_max, 2 * 1024u64.pow(3));
        assert_eq!(
            config.remote_cache.as_deref(),
            Some("s3://bucket/team/repo/")
        );
        assert_eq!(config.warmup_revisions, ["main", "release"]);
        assert!(config.s3_force_path_style);
        assert!(config.hash_options.bazel.verbose);
    }

    #[test]
    fn serve_config_parses_nexus_flags() {
        let serve_args = |extra: &[&str]| {
            let mut argv = vec![
                "bazel-diff",
                "serve",
                "--workspacePath",
                "/tmp/ws",
                "--cacheDir",
                "/tmp/cache",
            ];
            argv.extend_from_slice(extra);
            let Commands::Serve(args) = parse(&argv).command else {
                panic!("expected serve");
            };
            args
        };
        let nexus = [
            "--nexusUrl",
            "https://nexus.example.com/nexus/",
            "--nexusRepository",
            "bazel-diff",
            "--nexusPrefix",
            "/team/repo/",
            "--nexusCaCert",
            "/etc/ssl/../ssl/corp-ca.pem",
        ];

        let config = serve_args(&nexus)
            .to_config_with_nexus_password(false, None)
            .unwrap();
        assert_eq!(
            config.remote_cache.as_deref(),
            Some("https://nexus.example.com/nexus/repository/bazel-diff/team/repo/")
        );
        assert_eq!(config.nexus_repository.as_deref(), Some("bazel-diff"));
        assert_eq!(config.nexus_prefix, "/team/repo/");
        assert_eq!(
            config.nexus_ca_cert.as_deref(),
            Some(Path::new("/etc/ssl/corp-ca.pem"))
        );
        assert!(config.nexus_credentials.is_none());
        assert!(config.s3_bucket.is_none());

        let mut authenticated = nexus.to_vec();
        authenticated.extend(["--nexusUsername", "svc"]);
        let config = serve_args(&authenticated)
            .to_config_with_nexus_password(false, Some("s3cret".to_owned()))
            .unwrap();
        let credentials = config.nexus_credentials.unwrap();
        assert_eq!(credentials.username, "svc");
        assert_eq!(credentials.password, "s3cret");

        // A username without a password, or a password without a username, is a misconfiguration.
        assert!(serve_args(&authenticated)
            .to_config_with_nexus_password(false, None)
            .is_err());
        assert!(serve_args(&nexus)
            .to_config_with_nexus_password(false, Some("s3cret".to_owned()))
            .is_err());
        // The password environment variable is ignored when Nexus is not configured at all.
        assert!(serve_args(&[])
            .to_config_with_nexus_password(false, Some("s3cret".to_owned()))
            .unwrap()
            .remote_cache
            .is_none());
        // URL and repository problems surface at startup, not on the first cache lookup.
        assert!(
            serve_args(&["--nexusUrl", "ftp://host", "--nexusRepository", "cache"])
                .to_config_with_nexus_password(false, None)
                .is_err()
        );
        assert!(
            serve_args(&["--nexusUrl", "https://host", "--nexusRepository", "a/b"])
                .to_config_with_nexus_password(false, None)
                .is_err()
        );
    }

    #[test]
    fn serve_partial_graph_is_opt_in_and_validated() {
        let serve_args = |extra: &[&str]| {
            let mut argv = vec![
                "bazel-diff",
                "serve",
                "--workspacePath",
                "/tmp/ws",
                "--cacheDir",
                "/tmp/cache",
            ];
            argv.extend_from_slice(extra);
            let Commands::Serve(args) = parse(&argv).command else {
                panic!("expected serve");
            };
            args
        };
        let config = serve_args(&[]).to_config(false).unwrap();
        assert!(!config.partial_graph);
        assert!(config.partial_graph_rebuild_patterns.is_empty());
        assert!(!config.allow_untracked_packages);
        assert!(
            serve_args(&["--allowUntrackedPackages"])
                .to_config(false)
                .unwrap()
                .allow_untracked_packages
        );

        let config = serve_args(&[
            "--partialGraph",
            "--partialGraphRebuildPattern",
            "*.lock,tools/**",
            "--partialGraphRebuildPattern",
            "requirements.txt",
        ])
        .to_config(false)
        .unwrap();
        assert!(config.partial_graph);
        assert_eq!(
            config
                .partial_graph_rebuild_patterns
                .iter()
                .map(|glob| glob.pattern())
                .collect::<Vec<_>>(),
            ["*.lock", "tools/**", "requirements.txt"]
        );
        assert!(
            !serve_args(&["--partialGraph=false"])
                .to_config(false)
                .unwrap()
                .partial_graph
        );

        let error = serve_args(&["--partialGraph", "--useCquery"])
            .to_config(false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--useCquery"), "{error}");
        let error = serve_args(&["--partialGraph", "--alwaysAffectedTags", "flaky"])
            .to_config(false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--alwaysAffectedTags"), "{error}");
        assert!(
            serve_args(&["--partialGraph", "--partialGraphRebuildPattern", ""])
                .to_config(false)
                .is_err()
        );
    }

    #[test]
    fn serve_nexus_flags_require_each_other_and_conflict_with_s3() {
        for extra in [
            &["--nexusUrl", "https://host"][..],
            &["--nexusRepository", "cache"][..],
            &[
                "--nexusUrl",
                "https://host",
                "--nexusRepository",
                "cache",
                "--s3Bucket",
                "bucket",
            ][..],
        ] {
            let mut argv = vec![
                "bazel-diff",
                "serve",
                "--workspacePath",
                "/tmp/ws",
                "--cacheDir",
                "/tmp/cache",
            ];
            argv.extend_from_slice(extra);
            assert!(Cli::try_parse_from(&argv).is_err(), "{extra:?}");
        }
    }

    #[test]
    fn serve_dependency_fingerprint_guard_is_opt_in() {
        let serve_config = |extra: &[&str]| {
            let mut argv = vec![
                "bazel-diff",
                "serve",
                "--workspacePath",
                "/tmp/ws",
                "--cacheDir",
                "/tmp/cache",
            ];
            argv.extend_from_slice(extra);
            let Commands::Serve(args) = parse(&argv).command else {
                panic!("expected serve");
            };
            args.to_config(false).unwrap()
        };
        assert!(!serve_config(&[]).dependency_fingerprint);
        assert!(serve_config(&["--dependencyFingerprint"]).dependency_fingerprint);
        assert!(serve_config(&["--dependencyFingerprint=true"]).dependency_fingerprint);
        assert!(!serve_config(&["--dependencyFingerprint=false"]).dependency_fingerprint);
        assert!(!serve_config(&["--no-dependencyFingerprint"]).dependency_fingerprint);
    }

    #[test]
    fn serve_config_rejects_invalid_values_and_empty_endpoint() {
        for (flag, value) in [
            ("--cacheMaxAge", "7"),
            ("--cacheMaxSize", "huge"),
            ("--cachePruneInterval", "1hour"),
        ] {
            let cli = parse(&[
                "bazel-diff",
                "serve",
                "--workspacePath",
                "/tmp/ws",
                "--cacheDir",
                "/tmp/cache",
                flag,
                value,
            ]);
            let Commands::Serve(args) = cli.command else {
                panic!("expected serve");
            };
            assert!(args.to_config(false).is_err(), "{flag}={value}");
        }

        let cli = parse(&[
            "bazel-diff",
            "serve",
            "--workspacePath",
            "/tmp/ws",
            "--cacheDir",
            "/tmp/cache",
            "--s3Bucket",
            "bucket",
            "--s3Endpoint=",
        ]);
        let Commands::Serve(args) = cli.command else {
            panic!("expected serve");
        };
        assert!(args.to_config(false).is_err());
    }

    #[test]
    fn fingerprint_command_writes_file_and_changes_with_flags() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join(".bazelrc"), "common --x\n").unwrap();
        let first = workspace.path().join("first.json");
        let second = workspace.path().join("second.json");
        let mut hashing = hashing_args(workspace.path());
        hashing.bazel_path = PathBuf::from("/definitely/missing/bazel");
        run_fingerprint(&FingerprintArgs {
            hashing: hashing.clone(),
            include_target_type: false,
            target_type: Vec::new(),
            output: Some(first.clone()),
        })
        .unwrap();
        hashing.use_cquery = true;
        run_fingerprint(&FingerprintArgs {
            hashing,
            include_target_type: false,
            target_type: Vec::new(),
            output: Some(second.clone()),
        })
        .unwrap();
        let first: serde_json::Value = serde_json::from_slice(&fs::read(first).unwrap()).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&fs::read(second).unwrap()).unwrap();
        assert!(first.get("fingerprint").is_some());
        assert!(first.get("flags").is_some());
        assert!(first.get("components").is_some());
        assert_ne!(first["fingerprint"], second["fingerprint"]);
    }

    #[test]
    fn generate_filters_types_and_writes_dependency_edges() {
        let workspace = tempfile::tempdir().unwrap();
        let output = workspace.path().join("hashes.json");
        let deps = workspace.path().join("deps.json");
        let data = run_generate_with(
            &GenerateHashesArgs {
                hashing: hashing_args(workspace.path()),
                content_hash_path: None,
                include_target_type: true,
                target_type: vec!["Rule".into()],
                dep_edges_file: Some(deps.clone()),
                modified_filepaths: None,
                partial_graph: false,
                partial_graph_rebuild_patterns: Vec::new(),
                base_hashes: None,
                git_path: PathBuf::from("git"),
                output_path: Some(output.clone()),
            },
            false,
            |_| {
                Ok(HashFileData {
                    hashes: BTreeMap::from([
                        (
                            "//app:rule".into(),
                            bazel_diff::model::TargetHash {
                                kind: "Rule".into(),
                                hash: "rule".into(),
                                direct_hash: "direct".into(),
                            },
                        ),
                        (
                            "//app:source".into(),
                            bazel_diff::model::TargetHash {
                                kind: "SourceFile".into(),
                                hash: "source".into(),
                                direct_hash: "source".into(),
                            },
                        ),
                    ]),
                    dep_edges: BTreeMap::from([
                        ("//app:rule".into(), vec!["//app:source".into()]),
                        ("//app:source".into(), vec![]),
                    ]),
                    ..Default::default()
                })
            },
        )
        .unwrap();
        assert_eq!(data.hashes.keys().collect::<Vec<_>>(), ["//app:rule"]);
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(written["//app:rule"], "Rule#rule~direct");
        assert_eq!(
            fs::read(deps).unwrap(),
            br#"{"//app:rule":["//app:source"],"//app:source":[]}"#
        );
    }

    #[test]
    fn impacted_command_writes_text_and_distance_outputs_without_module_query() {
        let workspace = tempfile::tempdir().unwrap();
        let from_path = workspace.path().join("from.json");
        let to_path = workspace.path().join("to.json");
        let deps_path = workspace.path().join("deps.json");
        let text_output = workspace.path().join("impacted.txt");
        let distance_output = workspace.path().join("impacted.json");
        let target = |hash: &str, direct_hash: &str| bazel_diff::model::TargetHash {
            kind: "Rule".into(),
            hash: hash.into(),
            direct_hash: direct_hash.into(),
        };
        let from = HashFileData {
            hashes: BTreeMap::from([
                ("//app:leaf".into(), target("old", "old")),
                ("//app:top".into(), target("old-top", "same-top")),
            ]),
            ..Default::default()
        };
        let to = HashFileData {
            hashes: BTreeMap::from([
                ("//app:leaf".into(), target("new", "new")),
                ("//app:top".into(), target("new-top", "same-top")),
            ]),
            ..Default::default()
        };
        fs::write(
            &from_path,
            serde_json::to_vec(&from.serialized(true, false)).unwrap(),
        )
        .unwrap();
        fs::write(
            &to_path,
            serde_json::to_vec(&to.serialized(true, false)).unwrap(),
        )
        .unwrap();
        fs::write(
            &deps_path,
            serde_json::to_vec(&BTreeMap::from([("//app:top", vec!["//app:leaf"])])).unwrap(),
        )
        .unwrap();
        let base_args = || GetImpactedTargetsArgs {
            starting_hashes: from_path.clone(),
            final_hashes: to_path.clone(),
            dep_edges_file: None,
            target_type: vec!["Rule".into()],
            output: Some(text_output.clone()),
            workspace_path: workspace.path().to_path_buf(),
            bazel_path: PathBuf::from("/unused/bazel"),
            bazel_startup_options: vec!["--batch".into()],
            no_bazelrc: true,
            exclude_external_targets: Some(false),
        };

        run_get_impacted(&base_args(), true).unwrap();
        assert_eq!(
            fs::read_to_string(&text_output).unwrap(),
            "//app:leaf\n//app:top\n"
        );

        let mut distance_args = base_args();
        distance_args.dep_edges_file = Some(deps_path);
        distance_args.output = Some(distance_output.clone());
        run_get_impacted(&distance_args, false).unwrap();
        let values: Vec<bazel_diff::model::ImpactedTargetWithDistance> =
            serde_json::from_slice(&fs::read(distance_output).unwrap()).unwrap();
        assert_eq!(values[0].label, "//app:leaf");
        assert_eq!(values[0].target_distance, 0);
        assert_eq!(values[1].label, "//app:top");
        assert_eq!(values[1].target_distance, 1);
    }

    fn explain_fixture(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        // //common:util.py -> //common:lib -> //service/a:app
        let from = dir.join("from.json");
        let to = dir.join("to.json");
        let deps = dir.join("deps.json");
        fs::write(
            &from,
            r#"{"//service/a:app": "Rule#app~app-direct", "//common:lib": "Rule#lib~lib-direct", "//common:util.py": "SourceFile#util~util"}"#,
        )
        .unwrap();
        fs::write(
            &to,
            r#"{"//service/a:app": "Rule#app2~app-direct", "//common:lib": "Rule#lib2~lib-direct", "//common:util.py": "SourceFile#util2~util2"}"#,
        )
        .unwrap();
        fs::write(
            &deps,
            r#"{"//service/a:app": ["//common:lib"], "//common:lib": ["//common:util.py"]}"#,
        )
        .unwrap();
        (from, to, deps)
    }

    fn explain_args(dir: &Path, extra: &[&str]) -> ExplainArgs {
        let (from, to, deps) = explain_fixture(dir);
        let mut arguments = vec![
            "bazel-diff".to_owned(),
            "explain".to_owned(),
            "-sh".to_owned(),
            from.display().to_string(),
            "-fh".to_owned(),
            to.display().to_string(),
            "-d".to_owned(),
            deps.display().to_string(),
            "-t".to_owned(),
            "//service/a:app".to_owned(),
        ];
        arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));
        let cli = Cli::try_parse_from(arguments.into_iter().map(normalize_argument)).unwrap();
        let Commands::Explain(args) = cli.command else {
            panic!("expected explain");
        };
        args
    }

    #[test]
    fn explain_defaults_and_writes_the_chosen_format_to_the_output_file() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("explain.txt");

        let args = explain_args(dir.path(), &[]);
        assert_eq!(args.max_root_causes, 25);
        assert_eq!(args.max_depth, -1);
        assert_eq!(args.format, ExplainFormatArg::Text);

        let mut text = explain_args(dir.path(), &[]);
        text.output = Some(output.clone());
        run_explain(&text, false).unwrap();
        let rendered = fs::read_to_string(&output).unwrap();
        assert!(rendered.contains("IMPACTED (indirectly"));
        assert!(rendered.contains("//common:util.py -> //common:lib -> //service/a:app"));

        let mut dot = explain_args(dir.path(), &["--format", "DOT"]);
        dot.output = Some(output.clone());
        run_explain(&dot, false).unwrap();
        let rendered = fs::read_to_string(&output).unwrap();
        assert!(rendered.contains("digraph bazel_diff_impact {"));
        assert!(rendered.contains("(root cause)"));

        let mut mermaid = explain_args(dir.path(), &["-f", "mermaid"]);
        mermaid.output = Some(output.clone());
        run_explain(&mermaid, false).unwrap();
        assert!(fs::read_to_string(&output)
            .unwrap()
            .contains("classDef rootCause"));

        let mut json = explain_args(dir.path(), &["--format", "json"]);
        json.output = Some(output.clone());
        run_explain(&json, true).unwrap();
        let rendered = fs::read_to_string(&output).unwrap();
        assert!(rendered.contains("\"rootCauses\""));
        assert!(rendered.contains("\"packageHops\""));

        // util.py is 2 hops up; a 1-hop budget cannot reach it.
        let mut shallow = explain_args(dir.path(), &["--maxDepth", "1"]);
        shallow.output = Some(output.clone());
        run_explain(&shallow, false).unwrap();
        assert!(fs::read_to_string(&output)
            .unwrap()
            .contains("No root cause could be attributed"));
    }

    #[test]
    fn explain_reports_usage_errors_for_bad_inputs() {
        let dir = tempfile::tempdir().unwrap();

        let mut typo = explain_args(dir.path(), &[]);
        typo.target = "//typo:nope".to_owned();
        let error = run_explain(&typo, false).unwrap_err().to_string();
        assert!(
            error.contains("is not present in either hash file"),
            "{error}"
        );

        let mut missing_hashes = explain_args(dir.path(), &[]);
        missing_hashes.starting_hashes = dir.path().join("missing.json");
        let error = format!("{:#}", run_explain(&missing_hashes, false).unwrap_err());
        assert!(error.contains("Incorrect starting hashes"), "{error}");

        let mut missing_deps = explain_args(dir.path(), &[]);
        missing_deps.dep_edges_file = dir.path().join("missing-deps.json");
        let error = format!("{:#}", run_explain(&missing_deps, false).unwrap_err());
        assert!(error.contains("Incorrect dep edges file"), "{error}");
    }

    #[test]
    fn warmup_orders_generation_before_fingerprint_and_stops_on_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let args = WarmupArgs {
            generate: GenerateHashesArgs {
                hashing: hashing_args(workspace.path()),
                content_hash_path: None,
                include_target_type: true,
                target_type: vec!["Rule".into()],
                dep_edges_file: None,
                modified_filepaths: None,
                partial_graph: false,
                partial_graph_rebuild_patterns: Vec::new(),
                base_hashes: None,
                git_path: PathBuf::from("git"),
                output_path: None,
            },
            base_hashes: workspace.path().join("nested/base.json"),
            fingerprint_output: workspace.path().join("nested/fingerprint.json"),
        };
        let events = std::cell::RefCell::new(Vec::new());
        run_warmup_with(
            &args,
            |generate| {
                events.borrow_mut().push("generate");
                assert_eq!(generate.output_path.as_ref(), Some(&args.base_hashes));
                Ok(())
            },
            |fingerprint| {
                events.borrow_mut().push("fingerprint");
                assert_eq!(fingerprint.output.as_ref(), Some(&args.fingerprint_output));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(events.into_inner(), ["generate", "fingerprint"]);
        assert!(args.base_hashes.parent().unwrap().is_dir());

        let fingerprint_called = std::cell::Cell::new(false);
        assert!(run_warmup_with(
            &args,
            |_| Err(anyhow::anyhow!("generation failed")),
            |_| {
                fingerprint_called.set(true);
                Ok(())
            },
        )
        .is_err());
        assert!(!fingerprint_called.get());
    }

    fn git_in(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn partial_generate_args(workspace: &Path, output: &Path) -> GenerateHashesArgs {
        GenerateHashesArgs {
            hashing: hashing_args(workspace),
            content_hash_path: None,
            include_target_type: false,
            target_type: Vec::new(),
            dep_edges_file: None,
            modified_filepaths: None,
            partial_graph: true,
            partial_graph_rebuild_patterns: Vec::new(),
            base_hashes: None,
            git_path: PathBuf::from("git"),
            output_path: Some(output.to_path_buf()),
        }
    }

    /// A partial-graph entry for the repository's current tree whose only target is the source
    /// file `file.txt`, as a full generation in the mode would write it.
    fn partial_base(repo: &Path, path: &Path) -> HashFileData {
        let git = Git::new("git", repo);
        let tree = git.snapshot_working_tree(&[]).unwrap();
        let recipes = bazel_diff::partial::PartialRecipes {
            version: bazel_diff::partial::RECIPE_VERSION,
            seed_hash: "00".repeat(32),
            package_seeds: true,
            ..Default::default()
        };
        let targets = BTreeMap::from([("//:file.txt".to_owned(), "SourceFile".to_owned())]);
        let data = HashFileData {
            hashes: bazel_diff::partial::evaluate(&recipes, &targets, &tree).unwrap(),
            partial: Some(recipes),
            tree: tree.id().map(str::to_owned),
            ..Default::default()
        };
        fs::write(
            path,
            serde_json::to_vec(&data.serialized(true, false)).unwrap(),
        )
        .unwrap();
        data
    }

    #[test]
    fn generate_partial_graph_flags_parse() {
        let cli = parse(&[
            "bazel-diff",
            "generate-hashes",
            "-w",
            ".",
            "--partialGraph",
            "--partialGraphRebuildPattern",
            "*.lock,third_party/**",
            "--baseHashes",
            "base.json",
            "--gitPath",
            "/opt/git",
            "out.json",
        ]);
        let Commands::GenerateHashes(args) = cli.command else {
            panic!("expected generate-hashes");
        };
        assert!(args.partial_graph);
        assert_eq!(
            args.partial_graph_rebuild_patterns,
            ["*.lock", "third_party/**"]
        );
        assert_eq!(args.base_hashes, Some(PathBuf::from("base.json")));
        assert_eq!(args.git_path, PathBuf::from("/opt/git"));
        let Commands::GenerateHashes(defaults) =
            parse(&["bazel-diff", "generate-hashes", "-w", "."]).command
        else {
            panic!("expected generate-hashes");
        };
        assert!(!defaults.partial_graph);
        assert!(defaults.base_hashes.is_none());
        assert_eq!(defaults.git_path, PathBuf::from("git"));
    }

    #[test]
    fn generate_partial_graph_rejects_incompatible_options() {
        let workspace = tempfile::tempdir().unwrap();
        let output = workspace.path().join("out.json");
        let never = |_: &HashOptions| -> Result<HashFileData> { panic!("must not generate") };
        let error = |args: &GenerateHashesArgs| {
            run_generate_with(args, false, never)
                .unwrap_err()
                .to_string()
        };
        let mut cquery = partial_generate_args(workspace.path(), &output);
        cquery.hashing.use_cquery = true;
        assert!(error(&cquery).contains("--useCquery"));
        let mut content = partial_generate_args(workspace.path(), &output);
        content.content_hash_path = Some(workspace.path().join("content.json"));
        fs::write(workspace.path().join("content.json"), "{}").unwrap();
        assert!(error(&content).contains("--contentHashPath"));
        let mut modified = partial_generate_args(workspace.path(), &output);
        modified.modified_filepaths = Some(workspace.path().join("modified.txt"));
        fs::write(workspace.path().join("modified.txt"), "").unwrap();
        assert!(error(&modified).contains("--modified-filepaths"));
        let mut typed = partial_generate_args(workspace.path(), &output);
        typed.target_type = vec!["Rule".into()];
        assert!(error(&typed).contains("--targetType"));
        let mut pattern = partial_generate_args(workspace.path(), &output);
        pattern.partial_graph_rebuild_patterns = vec!["[".into()];
        assert!(!error(&pattern).is_empty());
        let mut without_mode = partial_generate_args(workspace.path(), &output);
        without_mode.partial_graph = false;
        without_mode.base_hashes = Some(workspace.path().join("base.json"));
        assert!(error(&without_mode).contains("--baseHashes needs --partialGraph"));
        // Not a git repository: the snapshot fails before any generation.
        assert!(!error(&partial_generate_args(workspace.path(), &output)).is_empty());
    }

    #[test]
    fn generate_partial_graph_derives_from_base_or_falls_back() {
        let repo = tempfile::tempdir().unwrap();
        git_in(repo.path(), &["init", "-q"]);
        git_in(repo.path(), &["config", "user.email", "test@example.com"]);
        git_in(repo.path(), &["config", "user.name", "Test"]);
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        git_in(repo.path(), &["add", "-A"]);
        git_in(repo.path(), &["commit", "-q", "-m", "one"]);
        let base_path = repo.path().join("base.json");
        let base = partial_base(repo.path(), &base_path);
        let output = repo.path().join("out.json");
        let never = |_: &HashOptions| -> Result<HashFileData> { panic!("must not generate") };
        let mut args = partial_generate_args(repo.path(), &output);
        args.base_hashes = Some(base_path.clone());

        // The base already describes the working tree: returned as is, no generation.
        let same = run_generate_with(&args, false, never).unwrap();
        assert_eq!(same.hashes, base.hashes);
        assert_eq!(same.tree, base.tree);

        // An uncommitted source edit is derived: no generation, a new tree, a moved hash.
        fs::write(repo.path().join("file.txt"), "two").unwrap();
        let derived = run_generate_with(&args, false, never).unwrap();
        assert_ne!(derived.tree, base.tree);
        assert_ne!(derived.hashes["//:file.txt"], base.hashes["//:file.txt"]);
        assert_eq!(derived.partial, base.partial);
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(written["metadata"]["tree"], derived.tree.clone().unwrap());
        assert!(
            written["hashes"]["//:file.txt"]
                .as_str()
                .unwrap()
                .starts_with("SourceFile#"),
            "partial-graph files always carry target types: {written}"
        );
        // The hash files inside the workspace are not part of the snapshot.
        let expected_tree = |excluded: &[&Path]| {
            Git::new("git", repo.path())
                .snapshot_working_tree(excluded)
                .unwrap()
                .id()
                .map(str::to_owned)
        };
        assert_eq!(derived.tree, expected_tree(&[&output, &base_path]));
        assert_ne!(derived.tree, expected_tree(&[]));

        // A graph-defining edit falls back to a full generation that hashes from the tree.
        fs::write(repo.path().join("BUILD"), "").unwrap();
        let generated = std::cell::Cell::new(false);
        let full = run_generate_with(&args, false, |options| {
            generated.set(true);
            let tree = options.partial_tree.as_ref().expect("tree for generation");
            assert!(tree.get("BUILD").is_some());
            assert_eq!(
                options.content_hashes.as_ref().unwrap()["file.txt"],
                tree.content_hashes()["file.txt"]
            );
            Ok(HashFileData {
                tree: tree.id().map(str::to_owned),
                ..Default::default()
            })
        })
        .unwrap();
        assert!(generated.get());
        assert_eq!(full.tree, expected_tree(&[&output, &base_path]));
        fs::remove_file(repo.path().join("BUILD")).unwrap();

        // A base without recipes or without a tree, and no base at all, all generate.
        let counted = |args: &GenerateHashesArgs| {
            let calls = std::cell::Cell::new(0);
            run_generate_with(args, false, |_| {
                calls.set(calls.get() + 1);
                Ok(HashFileData::default())
            })
            .unwrap();
            calls.get()
        };
        let mut no_recipes = base.clone();
        no_recipes.partial = None;
        fs::write(
            &base_path,
            serde_json::to_vec(&no_recipes.serialized(true, false)).unwrap(),
        )
        .unwrap();
        assert_eq!(counted(&args), 1);
        let mut with_edges = partial_generate_args(repo.path(), &output);
        with_edges.base_hashes = Some(base_path.clone());
        with_edges.dep_edges_file = Some(repo.path().join("deps.json"));
        fs::write(
            &base_path,
            serde_json::to_vec(&base.serialized(true, false)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            counted(&with_edges),
            1,
            "a base without edges cannot feed -d"
        );
        let mut edged = base.clone();
        edged.dep_edges = BTreeMap::from([("//:file.txt".to_owned(), vec![])]);
        fs::write(
            &base_path,
            serde_json::to_vec(&edged.serialized(true, true)).unwrap(),
        )
        .unwrap();
        assert_eq!(counted(&with_edges), 0, "a base with edges derives");
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(
            written["metadata"]["depEdges"]["//:file.txt"],
            serde_json::json!([])
        );
        assert_eq!(
            fs::read(repo.path().join("deps.json")).unwrap(),
            br#"{"//:file.txt":[]}"#
        );
        let mut no_tree = base.clone();
        no_tree.tree = None;
        fs::write(
            &base_path,
            serde_json::to_vec(&no_tree.serialized(true, false)).unwrap(),
        )
        .unwrap();
        assert_eq!(counted(&args), 1);
        let mut unknown_tree = base.clone();
        unknown_tree.tree = Some("0".repeat(40));
        fs::write(
            &base_path,
            serde_json::to_vec(&unknown_tree.serialized(true, false)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            counted(&args),
            1,
            "a base tree missing from the clone falls back"
        );
        assert_eq!(counted(&partial_generate_args(repo.path(), &output)), 1);
        // A base file that cannot be read is a misconfiguration, not a fallback.
        args.base_hashes = Some(repo.path().join("missing.json"));
        assert!(run_generate_with(&args, false, never)
            .unwrap_err()
            .to_string()
            .contains("missing.json"));
    }

    #[test]
    fn impacted_and_explain_refuse_mixed_modes() {
        let dir = tempfile::tempdir().unwrap();
        let explain = explain_args(dir.path(), &[]);
        let (from, to, deps) = explain_fixture(dir.path());
        let mut partial = HashFileData::read(&to).unwrap();
        partial.partial = Some(bazel_diff::partial::PartialRecipes::default());
        fs::write(
            &to,
            serde_json::to_vec(&partial.serialized(true, false)).unwrap(),
        )
        .unwrap();
        let error = run_get_impacted(
            &GetImpactedTargetsArgs {
                starting_hashes: from.clone(),
                final_hashes: to.clone(),
                dep_edges_file: Some(deps.clone()),
                target_type: Vec::new(),
                output: None,
                workspace_path: dir.path().to_path_buf(),
                bazel_path: PathBuf::from("/definitely/missing/bazel"),
                bazel_startup_options: Vec::new(),
                no_bazelrc: false,
                exclude_external_targets: Some(false),
            },
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("same mode"), "{error}");
        let error = run_explain(&explain, false).unwrap_err().to_string();
        assert!(error.contains("same mode"), "{error}");
    }
}
