use anyhow::{bail, Context, Result};
use bazel_diff::bazel::BazelOptions;
use bazel_diff::fingerprint;
use bazel_diff::hash::{generate_hashes, HashOptions};
use bazel_diff::model::{
    filter_and_sort_labels, impacted_targets, impacted_targets_with_distances, HashFileData,
};
use bazel_diff::module_graph::impacted_with_module_changes;
use clap::{ArgAction, Args, Parser, Subcommand};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    #[command(about = "Warm Bazel and write snapshot hashes and fingerprint metadata")]
    Warmup(WarmupArgs),
    #[command(about = "Compute the snapshot/cache fingerprint for the current workspace")]
    Fingerprint(FingerprintArgs),
    #[command(about = "Run the HTTP impacted-target query service")]
    Serve(ServeArgs),
}

#[derive(Clone, Debug, Args)]
struct HashingArgs {
    #[arg(
        short = 'w',
        long = "workspacePath",
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

    #[arg(short = 'w', long = "workspacePath")]
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

    #[arg(long = "cacheDir")]
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

    #[arg(long = "s3Bucket")]
    s3_bucket: Option<String>,

    #[arg(long = "s3Prefix", default_value = "")]
    s3_prefix: String,

    #[arg(long = "s3Region")]
    s3_region: Option<String>,

    #[arg(long = "s3Endpoint")]
    s3_endpoint: Option<String>,

    #[arg(long = "s3ForcePathStyle")]
    s3_force_path_style: bool,
}

fn flatten_options(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(' '))
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn read_lines(path: Option<&Path>) -> Result<BTreeSet<PathBuf>> {
    let Some(path) = path else {
        return Ok(BTreeSet::new());
    };
    Ok(fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn read_string_lines(path: &Path) -> Result<BTreeSet<String>> {
    Ok(fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .lines()
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
        exclude_external_targets: args.exclude_external_targets,
        exclude_targets_query: args.exclude_targets_query.clone(),
        no_bazelrc: false,
        verbose,
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

fn write_json(path: Option<&Path>, value: &impl Serialize) -> Result<()> {
    match path {
        Some(path) if path != Path::new("-") => {
            let mut writer = BufWriter::new(
                File::create(path)
                    .with_context(|| format!("failed to create {}", path.display()))?,
            );
            serde_json::to_writer(&mut writer, value)
                .with_context(|| format!("failed to write {}", path.display()))?;
            writer
                .flush()
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        _ => {
            let stdout = io::stdout();
            let mut writer = stdout.lock();
            serde_json::to_writer(&mut writer, value)?;
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn run_generate(args: &GenerateHashesArgs, verbose: bool) -> Result<HashFileData> {
    let options = hash_options(
        &args.hashing,
        args.content_hash_path.as_deref(),
        args.modified_filepaths.as_deref(),
        args.dep_edges_file.is_some(),
        verbose,
    )?;
    let mut data = generate_hashes(&options)?;
    if !args.target_type.is_empty() {
        let types = args.target_type.iter().cloned().collect::<HashSet<_>>();
        data.hashes.retain(|_, hash| types.contains(&hash.kind));
    }
    write_json(
        args.output_path.as_deref(),
        &data.serialized(args.include_target_type, false),
    )?;
    if let Some(path) = &args.dep_edges_file {
        fs::write(path, serde_json::to_vec(&data.dep_edges)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(data)
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
        exclude_external_targets: false,
        exclude_targets_query: None,
        no_bazelrc: args.no_bazelrc,
        verbose,
    }
}

fn run_get_impacted(args: &GetImpactedTargetsArgs, verbose: bool) -> Result<()> {
    let from = HashFileData::read(&args.starting_hashes)?;
    let to = HashFileData::read(&args.final_hashes)?;
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
        output_path: Some(args.base_hashes.clone()),
    };
    generate.output_path = Some(args.base_hashes.clone());
    run_generate(&generate, verbose)?;
    if let Some(parent) = args.fingerprint_output.parent() {
        fs::create_dir_all(parent)?;
    }
    let fingerprint_args = FingerprintArgs {
        hashing: args.generate.hashing.clone(),
        include_target_type: args.generate.include_target_type,
        target_type: args.generate.target_type.clone(),
        output: Some(args.fingerprint_output.clone()),
    };
    run_fingerprint(&fingerprint_args)
}

impl ServeArgs {
    fn to_config(&self, verbose: bool) -> Result<bazel_diff::server::ServerConfig> {
        if self.s3_bucket.is_some() && self.s3_endpoint.as_deref().is_some_and(str::is_empty) {
            bail!("--s3Endpoint must not be empty");
        }
        let remote_cache = self.s3_bucket.as_ref().map(|bucket| {
            let prefix = self.s3_prefix.trim_matches('/');
            if prefix.is_empty() {
                format!("s3://{bucket}/")
            } else {
                format!("s3://{bucket}/{prefix}/")
            }
        });
        Ok(bazel_diff::server::ServerConfig {
            hash_options: hash_options(&self.hashing, None, None, self.track_deps, verbose)?,
            git_path: self.git_path.clone(),
            port: self.port,
            request_timeout: Duration::from_secs(self.request_timeout),
            cache_dir: self.cache_dir.clone(),
            track_deps: self.track_deps,
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
            remote_cache,
            s3_bucket: self.s3_bucket.clone(),
            s3_prefix: self.s3_prefix.clone(),
            s3_region: self.s3_region.clone(),
            s3_endpoint: self.s3_endpoint.clone(),
            s3_force_path_style: self.s3_force_path_style,
        })
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
}
