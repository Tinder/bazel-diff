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

fn parse_normalized_path(value: &str) -> Result<PathBuf, String> {
    Ok(normalize_path(PathBuf::from(value)))
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
    run_generate_with(args, verbose, generate_hashes)
}

fn run_generate_with(
    args: &GenerateHashesArgs,
    verbose: bool,
    generate: impl FnOnce(&HashOptions) -> Result<HashFileData>,
) -> Result<HashFileData> {
    let options = hash_options(
        &args.hashing,
        args.content_hash_path.as_deref(),
        args.modified_filepaths.as_deref(),
        args.dep_edges_file.is_some(),
        verbose,
    )?;
    let mut data = generate(&options)?;
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

    /// Reproducer for <https://github.com/Tinder/bazel-diff/issues/470>: the
    /// root cause.
    ///
    /// `--workspacePath` runs through [`parse_normalized_path`], which folds
    /// every `CurDir` component away. A path that is *made* of `CurDir`
    /// components -- `.`, `./`, or anything that cancels out via `..` --
    /// therefore normalizes to the empty `PathBuf` rather than to the
    /// directory the user named.
    ///
    /// The same parser backs `--cacheDir`, so it has the same hole.
    ///
    /// Ignored so CI stays green until this is fixed. Run it with
    /// `cargo test --bin bazel-diff -- --ignored issue_470`.
    #[test]
    #[ignore = "reproduces Tinder/bazel-diff#470: relative --workspacePath normalizes to the empty path"]
    fn issue_470_relative_workspace_path_survives_normalization() {
        for value in [".", "./", "./nested/..", "nested/.."] {
            let parsed = parse_normalized_path(value).unwrap();
            assert_ne!(
                parsed,
                PathBuf::new(),
                "--workspacePath {value} normalized to the empty path"
            );
        }

        // A relative path that does not cancel out is left relative, and works,
        // which is why the empty-path case reads as an unrelated failure.
        assert_eq!(
            parse_normalized_path("nested").unwrap(),
            PathBuf::from("nested")
        );
    }

    /// Reproducer for <https://github.com/Tinder/bazel-diff/issues/470>: the
    /// symptom a user actually sees.
    ///
    /// [`BazelOptions::command`] does `command.current_dir(&self.workspace)`.
    /// With the empty workspace from the test above, the child's `chdir("")`
    /// fails with `ENOENT`, and `std::process::Command` reports that as a spawn
    /// failure -- so the error names the *Bazel binary*, which exists and is
    /// executable, instead of the workspace:
    ///
    /// ```text
    /// [Error] failed to execute /path/to/bazel: No such file or directory (os error 2)
    /// ```
    ///
    /// Expected: `-w .` either resolves against the process working directory
    /// (as the Kotlin CLI does) or is rejected with a message naming
    /// `--workspacePath`.
    ///
    /// Ignored so CI stays green until this is fixed. Run it with
    /// `cargo test --bin bazel-diff -- --ignored issue_470`.
    #[cfg(unix)]
    #[test]
    #[ignore = "reproduces Tinder/bazel-diff#470: `-w .` makes every Bazel spawn fail with ENOENT"]
    fn issue_470_relative_workspace_path_spawns_bazel_in_the_workspace() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let directory = tempfile::tempdir().unwrap();
        let stub_bazel = directory.path().join("bazel");
        fs::write(&stub_bazel, "#!/bin/sh\necho 'Build label: 8.0.0'\n").unwrap();
        fs::set_permissions(&stub_bazel, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(stub_bazel.is_file(), "the stub Bazel exists");

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

        // Mirrors BazelOptions::command(): the only difference from a working
        // run is the workspace the child is chdir'd into.
        let spawned = Command::new(&options.bazel)
            .current_dir(&options.workspace)
            .arg("version")
            .output();

        match spawned {
            Ok(output) => assert!(
                output.status.success(),
                "stub Bazel should have run: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => panic!(
                "`-w .` gave workspace {:?}, so spawning {} failed: {error}",
                options.workspace,
                options.bazel.display()
            ),
        }
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
        let mut second = first.clone();
        second.fine_grained_hash_external_repos.reverse();
        second.ignored_rule_hashing_attributes.reverse();

        let first_flags = fingerprint_flags(&first, true, &["Rule".into(), "GeneratedFile".into()]);
        let second_flags =
            fingerprint_flags(&second, true, &["GeneratedFile".into(), "Rule".into()]);
        assert_eq!(first_flags, second_flags);
        assert_eq!(first_flags["bazelStartupOptions"], "--a --b");
        assert_eq!(first_flags["targetType"], "GeneratedFile,Rule");
        assert_eq!(first_flags["fineGrainedHashExternalRepos"], "abc,maven");
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
            "--requestTimeout",
            "30",
            "--cacheMaxAge",
            "7d",
            "--cacheMaxSize",
            "10gb",
            "--cacheMaxEntries",
            "500",
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
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.cache_max_age, Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(config.cache_max_size, Some(10 * 1024u64.pow(3)));
        assert_eq!(config.cache_max_entries, Some(500));
        assert_eq!(config.cache_prune_interval, Duration::from_secs(3600));
        assert_eq!(
            config.remote_cache.as_deref(),
            Some("s3://bucket/team/repo/")
        );
        assert_eq!(config.warmup_revisions, ["main", "release"]);
        assert!(config.s3_force_path_style);
        assert!(config.hash_options.bazel.verbose);
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
                                deps: vec!["//app:source".into()],
                            },
                        ),
                        (
                            "//app:source".into(),
                            bazel_diff::model::TargetHash {
                                kind: "SourceFile".into(),
                                hash: "source".into(),
                                direct_hash: "source".into(),
                                deps: Vec::new(),
                            },
                        ),
                    ]),
                    dep_edges: BTreeMap::from([("//app:rule".into(), vec!["//app:source".into()])]),
                    ..Default::default()
                })
            },
        )
        .unwrap();
        assert_eq!(data.hashes.keys().collect::<Vec<_>>(), ["//app:rule"]);
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(written["//app:rule"], "Rule#rule~direct");
        assert!(fs::read_to_string(deps).unwrap().contains("//app:source"));
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
            deps: Vec::new(),
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
}
