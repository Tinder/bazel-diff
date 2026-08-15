use crate::bazel::{encode_configured_input, BazelOptions};
use crate::model::{HashFileData, TargetHash};
use crate::proto::blaze_query::{target, Rule, Target};
use anyhow::{anyhow, bail, Context, Result};
use buffa::{EncodeSink, Message, SizeCache};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

const CONFIGURED_INPUT_SEPARATOR: char = '|';
type DigestBytes = [u8; 32];
const DEFAULT_HASH_THREADS: usize = 4;
const FILE_HASH_BUFFER_SIZE: usize = 64 * 1024;

fn hash_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("RAYON_NUM_THREADS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|threads| *threads > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1)
                    .min(DEFAULT_HASH_THREADS)
            });
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("bazel-diff-hash-{index}"))
            .build()
            .expect("build hashing thread pool")
    })
}

#[derive(Clone, Debug)]
pub struct HashOptions {
    pub bazel: BazelOptions,
    pub content_hashes: Option<BTreeMap<String, String>>,
    pub ignored_attributes: HashSet<String>,
    pub seed_filepaths: BTreeSet<PathBuf>,
    pub modified_filepaths: BTreeSet<PathBuf>,
    pub track_deps: bool,
    pub always_affected_tags: HashSet<String>,
}

#[derive(Clone, Debug)]
struct TargetDigest {
    overall: DigestBytes,
    direct: DigestBytes,
    deps: Vec<String>,
}

struct TargetDigestBuilder {
    overall: Sha256,
    direct: Sha256,
    deps: Vec<String>,
    track_deps: bool,
}

impl TargetDigestBuilder {
    fn new(track_deps: bool) -> Self {
        Self {
            overall: Sha256::new(),
            direct: Sha256::new(),
            deps: Vec::new(),
            track_deps,
        }
    }

    fn direct(&mut self, bytes: impl AsRef<[u8]>) {
        self.direct.update(bytes);
    }

    fn transitive(&mut self, label: &str, bytes: impl AsRef<[u8]>) {
        self.overall.update(bytes);
        if self.track_deps {
            self.deps.push(label.to_owned());
        }
    }

    fn finish(mut self) -> TargetDigest {
        let direct = self.direct.finalize().into();
        self.overall.update(direct);
        TargetDigest {
            overall: self.overall.finalize().into(),
            direct,
            deps: self.deps,
        }
    }
}

enum HashTarget {
    Rule(String),
    Generated {
        name: String,
        generating_rule: String,
    },
    Source(String),
}

struct SourceTarget {
    name: String,
    subincludes: Vec<String>,
}

pub fn generate_hashes(options: &HashOptions) -> Result<HashFileData> {
    let mut definition_digests = HashMap::new();
    let targets = options.bazel.query_all_targets_with(|target| {
        let Some(rule) = target.rule.as_option_mut() else {
            return;
        };
        definition_digests.insert(
            rule.name.clone(),
            prehash_rule(rule, &options.ignored_attributes),
        );
    })?;
    hash_targets_with_digests(options, targets, definition_digests)
}

pub fn hash_targets(options: &HashOptions, targets: Vec<Target>) -> Result<HashFileData> {
    hash_targets_with_digests(options, targets, HashMap::new())
}

fn hash_targets_with_digests(
    options: &HashOptions,
    targets: Vec<Target>,
    definition_digests: HashMap<String, DigestBytes>,
) -> Result<HashFileData> {
    let external_resolver = ExternalRepoResolver::new(&options.bazel)?;
    let module_graph_json = options.bazel.module_graph_json();
    hash_targets_with_environment(
        options,
        targets,
        definition_digests,
        external_resolver,
        module_graph_json,
    )
}

fn hash_targets_with_environment(
    options: &HashOptions,
    targets: Vec<Target>,
    mut definition_digests: HashMap<String, DigestBytes>,
    external_resolver: ExternalRepoResolver,
    module_graph_json: Option<String>,
) -> Result<HashFileData> {
    let source_hasher = SourceHasher {
        workspace: &options.bazel.workspace,
        content_hashes: options.content_hashes.as_ref(),
        modified_filepaths: &options.modified_filepaths,
        fine_grained_external_repos: options
            .bazel
            .fine_grained_external_repos
            .iter()
            .map(|name| name.trim_start_matches('@').to_owned())
            .collect(),
        external_resolver,
    };
    let seed_hash = hash_seed_files(&options.bazel.workspace, &options.seed_filepaths)?;
    let has_instantiation_stacks = targets.iter().any(|target| {
        target.r#type == target::Discriminator::Rule
            && target
                .rule
                .as_option()
                .is_some_and(|rule| !rule.instantiation_stack.is_empty())
    });
    let package_bzl_seeds = if has_instantiation_stacks {
        BTreeMap::new()
    } else {
        package_bzl_seeds(&targets, &source_hasher)?
    };

    let mut decoded_rules = Vec::<(String, Rule, Option<DigestBytes>)>::new();
    let mut source_targets = Vec::new();
    let mut hash_targets = Vec::with_capacity(targets.len());
    for target in targets {
        match target.r#type {
            target::Discriminator::Rule => {
                if let Some(rule) = target.rule.into_option() {
                    let name = rule.name.clone();
                    hash_targets.push(HashTarget::Rule(name.clone()));
                    let definition_digest = definition_digests.remove(&name);
                    decoded_rules.push((name, rule, definition_digest));
                }
            }
            target::Discriminator::GeneratedFile => {
                if let Some(file) = target.generated_file.into_option() {
                    hash_targets.push(HashTarget::Generated {
                        name: file.name,
                        generating_rule: file.generating_rule,
                    });
                }
            }
            target::Discriminator::SourceFile => {
                if let Some(source) = target.source_file.into_option() {
                    let name = source.name;
                    hash_targets.push(HashTarget::Source(name.clone()));
                    source_targets.push(SourceTarget {
                        name,
                        subincludes: source.subinclude,
                    });
                }
            }
            _ => {}
        }
    }
    let source_digests = hash_pool()
        .install(|| {
            source_targets
                .par_iter()
                .map(|source| {
                    let seed = source_seed(source);
                    source_hasher
                        .digest(&source.name, &seed)
                        .map(|digest| (source.name.clone(), digest))
                })
                .collect::<Result<Vec<_>>>()
        })?
        .into_iter()
        .collect::<HashMap<_, _>>();
    drop(source_targets);
    let processed_rules = hash_pool().install(|| {
        decoded_rules
            .into_par_iter()
            .map(|(name, rule, definition_digest)| {
                let definition_digest = definition_digest
                    .unwrap_or_else(|| rule_definition_digest(&rule, &options.ignored_attributes));
                (
                    name,
                    RuleNode {
                        rule,
                        definition_digest,
                    },
                )
            })
            .collect::<Vec<_>>()
    });
    let mut rules = HashMap::<String, Rc<RuleNode>>::with_capacity(processed_rules.len());
    for (name, node) in processed_rules {
        rules.insert(name, Rc::new(node));
    }
    for target in &hash_targets {
        let HashTarget::Generated {
            name,
            generating_rule,
        } = target
        else {
            continue;
        };
        let rule = rules
            .get(generating_rule)
            .cloned()
            .ok_or_else(|| anyhow!("Unexpected generating rule {generating_rule}"))?;
        rules.insert(name.clone(), rule);
    }

    let mut context = HashContext {
        options,
        source_hasher: &source_hasher,
        source_digests,
        rules,
        cache: HashMap::new(),
        seed_hash,
        package_bzl_seeds,
        bzl_digests: HashMap::new(),
        always_affected_seed: unique_invocation_seed(),
    };
    let mut hashes = BTreeMap::new();
    for target in hash_targets {
        let (label, kind, digest) = match target {
            HashTarget::Rule(label) => {
                let digest = context.rule_digest(&label, &mut Vec::new())?;
                (label, "Rule", digest)
            }
            HashTarget::Generated {
                name,
                generating_rule,
            } => {
                let mut digest = context.rule_digest(&generating_rule, &mut Vec::new())?;
                digest.deps = if options.track_deps {
                    vec![generating_rule]
                } else {
                    Vec::new()
                };
                (name, "GeneratedFile", digest)
            }
            HashTarget::Source(label) => {
                let mut hasher = Sha256::new();
                if let Some(digest) = context.source_digests.get(&label) {
                    hasher.update(digest);
                }
                hasher.update(context.seed_hash);
                if let Some(seed) = context.package_bzl_seeds.get(label_package(&label)) {
                    hasher.update(seed);
                }
                let digest = hasher.finalize().into();
                (
                    label,
                    "SourceFile",
                    TargetDigest {
                        overall: digest,
                        direct: digest,
                        deps: Vec::new(),
                    },
                )
            }
        };
        hashes.insert(
            label,
            TargetHash {
                kind: kind.to_owned(),
                hash: digest_hex(digest.overall),
                direct_hash: digest_hex(digest.direct),
                deps: digest.deps,
            },
        );
    }
    let dep_edges = if options.track_deps {
        hashes
            .iter()
            .map(|(label, hash)| (label.clone(), hash.deps.clone()))
            .collect()
    } else {
        BTreeMap::new()
    };
    Ok(HashFileData {
        hashes,
        module_graph_json,
        dep_edges,
    })
}

struct HashContext<'a> {
    options: &'a HashOptions,
    source_hasher: &'a SourceHasher<'a>,
    source_digests: HashMap<String, DigestBytes>,
    rules: HashMap<String, Rc<RuleNode>>,
    cache: HashMap<String, TargetDigest>,
    seed_hash: DigestBytes,
    package_bzl_seeds: BTreeMap<String, DigestBytes>,
    bzl_digests: HashMap<String, Option<DigestBytes>>,
    always_affected_seed: DigestBytes,
}

impl HashContext<'_> {
    fn rule_digest(&mut self, label: &str, path: &mut Vec<String>) -> Result<TargetDigest> {
        if let Some(digest) = self.cache.get(label) {
            return Ok(digest.clone());
        }
        let rule = self
            .rules
            .get(label)
            .cloned()
            .ok_or_else(|| anyhow!("Unable to find rule {label}"))?;
        let canonical_label = rule.rule.name.clone();
        if let Some(digest) = self.cache.get(&canonical_label) {
            return Ok(digest.clone());
        }
        if let Some(index) = path.iter().position(|entry| entry == &canonical_label) {
            let cycle = path[index..]
                .iter()
                .chain(std::iter::once(&canonical_label))
                .cloned()
                .collect::<Vec<_>>()
                .join(" -> ");
            bail!("Circular dependency detected: {cycle}");
        }
        path.push(canonical_label.clone());

        let mut builder = TargetDigestBuilder::new(self.options.track_deps);
        builder.direct(rule.definition_digest);
        builder.direct(self.seed_hash);
        if let Some(seed) = self.rule_bzl_seed(&rule.rule)? {
            builder.direct(seed);
        } else if let Some(seed) = self.package_bzl_seeds.get(label_package(&rule.rule.name)) {
            builder.direct(seed);
        }
        if !self.options.always_affected_tags.is_empty()
            && rule_tags(&rule.rule)
                .iter()
                .any(|tag| self.options.always_affected_tags.contains(*tag))
        {
            builder.direct(self.always_affected_seed);
        }

        for encoded_input in rule_inputs(
            &rule.rule,
            self.options.bazel.use_cquery,
            &self.options.bazel.fine_grained_external_repos,
        ) {
            builder.direct(encoded_input.as_bytes());
            let input_label = encoded_input
                .split_once(CONFIGURED_INPUT_SEPARATOR)
                .map_or(encoded_input.as_ref(), |(label, _)| label);
            if let Some(source_digest) = self.source_digests.get(input_label) {
                builder.direct(source_digest);
            } else if self.rules.contains_key(input_label) && input_label != canonical_label {
                let digest = self.rule_digest(input_label, path)?;
                builder.transitive(input_label, digest.overall);
            } else if let Some(digest) = self.source_hasher.soft_digest(input_label, &[])? {
                self.source_digests.insert(input_label.to_owned(), digest);
                builder.direct(digest);
            } else if self.options.bazel.verbose {
                eprintln!(
                    "[Warn] Unable to calculate digest for input {input_label} for rule {}",
                    rule.rule.name
                );
            }
        }

        if !self.options.ignored_attributes.contains("visibility") {
            for package_group in visibility_package_groups(&rule.rule) {
                if package_group == canonical_label {
                    continue;
                }
                let Some(group) = self.rules.get(package_group) else {
                    continue;
                };
                if group.rule.rule_class != "package_group" {
                    continue;
                }
                builder.direct(package_group.as_bytes());
                let digest = self.rule_digest(package_group, path)?;
                builder.transitive(package_group, digest.overall);
            }
        }

        path.pop();
        let digest = builder.finish();
        self.cache.insert(canonical_label, digest.clone());
        Ok(digest)
    }

    fn rule_bzl_seed(&mut self, rule: &Rule) -> Result<Option<DigestBytes>> {
        if rule.instantiation_stack.is_empty() {
            return Ok(None);
        }
        let paths = rule
            .instantiation_stack
            .iter()
            .filter_map(|frame| frame.split(':').next())
            .filter(|path| {
                (path.ends_with(".bzl") || path.ends_with(".scl"))
                    && !Path::new(path).is_absolute()
                    && !path.starts_with("external/")
                    && !path.starts_with('@')
                    && !path.starts_with("../")
            })
            .collect::<BTreeSet<_>>();
        let mut hasher = Sha256::new();
        for path in paths {
            let digest = if let Some(cached) = self.bzl_digests.get(path) {
                *cached
            } else {
                let digest = self.source_hasher.soft_digest(&format!("//{path}"), &[])?;
                self.bzl_digests.insert(path.to_owned(), digest);
                digest
            };
            if let Some(digest) = digest {
                hasher.update(path.as_bytes());
                hasher.update(digest);
            }
        }
        Ok(Some(hasher.finalize().into()))
    }
}

struct RuleNode {
    rule: Rule,
    definition_digest: DigestBytes,
}

fn source_seed(source: &SourceTarget) -> DigestBytes {
    let mut hasher = Sha256::new();
    hasher.update(source.name.as_bytes());
    for subinclude in &source.subincludes {
        hasher.update(subinclude.as_bytes());
    }
    hasher.finalize().into()
}

fn hash_seed_files(workspace: &Path, paths: &BTreeSet<PathBuf>) -> Result<DigestBytes> {
    let mut hasher = Sha256::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            workspace.join(path)
        };
        hash_file_into(&mut hasher, &absolute)
            .with_context(|| format!("failed to read seed file {}", absolute.display()))?;
    }
    Ok(hasher.finalize().into())
}

fn hash_file_into(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let mut buffer = [0; FILE_HASH_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..read]);
    }
}

fn unique_invocation_seed() -> DigestBytes {
    let mut hasher = Sha256::new();
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    hasher.finalize().into()
}

fn package_bzl_seeds(
    targets: &[Target],
    source_hasher: &SourceHasher<'_>,
) -> Result<BTreeMap<String, DigestBytes>> {
    let mut packages = BTreeMap::<String, BTreeSet<String>>::new();
    for target in targets {
        let Some(source) = target.source_file.as_option() else {
            continue;
        };
        if source.subinclude.is_empty() {
            continue;
        }
        packages
            .entry(label_package(&source.name).to_owned())
            .or_default()
            .extend(source.subinclude.iter().cloned());
    }
    let mut result = BTreeMap::new();
    for (package, labels) in packages {
        let mut hasher = Sha256::new();
        let mut tracked = false;
        for label in labels {
            if let Some(digest) = source_hasher.soft_digest(&label, &[])? {
                tracked = true;
                hasher.update(label.as_bytes());
                hasher.update(digest);
            }
        }
        if tracked {
            result.insert(package, hasher.finalize().into());
        }
    }
    Ok(result)
}

fn digest_hex(digest: DigestBytes) -> String {
    let mut encoded = [0; 64];
    hex::encode_to_slice(digest, &mut encoded).expect("fixed-size hex output");
    String::from_utf8(encoded.to_vec()).expect("hex output is ASCII")
}

struct Sha256Sink<'a>(&'a mut Sha256);

impl EncodeSink for Sha256Sink<'_> {
    fn put_u8(&mut self, value: u8) {
        self.0.update([value]);
    }

    fn put_slice(&mut self, src: &[u8]) {
        self.0.update(src);
    }

    fn put_u32_le(&mut self, value: u32) {
        self.0.update(value.to_le_bytes());
    }

    fn put_u64_le(&mut self, value: u64) {
        self.0.update(value.to_le_bytes());
    }
}

fn rule_definition_digest(rule: &Rule, ignored: &HashSet<String>) -> DigestBytes {
    let mut hasher = Sha256::new();
    hasher.update(rule.rule_class.as_bytes());
    hasher.update(rule.name.as_bytes());
    if let Some(environment_hash) = &rule.skylark_environment_hash_code {
        hasher.update(environment_hash.as_bytes());
    }
    let mut size_cache = SizeCache::new();
    let attributes_are_sorted = rule
        .attribute
        .windows(2)
        .all(|pair| pair[0].name <= pair[1].name);
    if attributes_are_sorted {
        for attribute in &rule.attribute {
            hash_attribute(attribute, ignored, &mut size_cache, &mut hasher);
        }
    } else {
        let mut attributes = rule.attribute.iter().collect::<Vec<_>>();
        attributes.sort_by_key(|attribute| &attribute.name);
        for attribute in attributes {
            hash_attribute(attribute, ignored, &mut size_cache, &mut hasher);
        }
    }
    hasher.finalize().into()
}

fn prehash_rule(rule: &mut Rule, ignored: &HashSet<String>) -> DigestBytes {
    let digest = rule_definition_digest(rule, ignored);
    rule.attribute
        .retain(|attribute| attribute.name == "tags" || attribute.name == "visibility");
    digest
}

fn hash_attribute(
    attribute: &crate::proto::blaze_query::Attribute,
    ignored: &HashSet<String>,
    size_cache: &mut SizeCache,
    hasher: &mut Sha256,
) {
    if attribute.name == "generator_location" || ignored.contains(&attribute.name) {
        return;
    }
    attribute.encode_with_cache(size_cache, &mut Sha256Sink(hasher));
}

fn rule_inputs<'a>(
    rule: &'a Rule,
    use_cquery: bool,
    fine_grained_repos: &BTreeSet<String>,
) -> Vec<Cow<'a, str>> {
    let external_synthetic = rule
        .rule_input
        .iter()
        .map(|input| transform_external_input(input, fine_grained_repos))
        .filter(|input| input.starts_with("//external:"));
    let mut inputs = if use_cquery {
        rule.configured_rule_input
            .iter()
            .map(|input| Cow::Owned(encode_configured_input(input)))
            .chain(external_synthetic)
            .collect::<Vec<_>>()
    } else {
        rule.rule_input
            .iter()
            .map(|input| Cow::Borrowed(input.as_str()))
            .chain(external_synthetic)
            .collect::<Vec<_>>()
    };
    inputs.sort_unstable();
    inputs.dedup();
    inputs
}

fn transform_external_input<'a>(
    input: &'a str,
    fine_grained_repos: &BTreeSet<String>,
) -> Cow<'a, str> {
    let is_main = input.starts_with("//") || input.starts_with("@//") || input.starts_with("@@//");
    if !is_main
        && input.starts_with('@')
        && !fine_grained_repos
            .iter()
            .any(|repo| input.starts_with(repo))
    {
        if let Some((repo, _)) = input.split_once("//") {
            return Cow::Owned(format!("//external:{}", repo.trim_start_matches('@')));
        }
    }
    Cow::Borrowed(input)
}

fn rule_tags(rule: &Rule) -> HashSet<&str> {
    rule.attribute
        .iter()
        .find(|attribute| attribute.name == "tags")
        .map(|attribute| {
            attribute
                .string_list_value
                .iter()
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default()
}

fn visibility_package_groups(rule: &Rule) -> Vec<&str> {
    let mut groups = rule
        .attribute
        .iter()
        .find(|attribute| attribute.name == "visibility")
        .map(|attribute| {
            attribute
                .string_list_value
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    groups.retain(|label| {
        !label.starts_with("//visibility:")
            && !label.ends_with(":__pkg__")
            && !label.ends_with(":__subpackages__")
    });
    groups.sort_unstable();
    groups.dedup();
    groups
}

pub fn label_package(label: &str) -> &str {
    label.rsplit_once(':').map_or(label, |(package, _)| package)
}

struct SourceHasher<'a> {
    workspace: &'a Path,
    content_hashes: Option<&'a BTreeMap<String, String>>,
    modified_filepaths: &'a BTreeSet<PathBuf>,
    fine_grained_external_repos: BTreeSet<String>,
    external_resolver: ExternalRepoResolver,
}

impl SourceHasher<'_> {
    fn digest(&self, label: &str, seed: &[u8]) -> Result<DigestBytes> {
        let Some((path, content_key)) = self.resolve(label)? else {
            return Ok(Sha256::digest([]).into());
        };
        let absolute = if path.is_absolute() {
            path
        } else {
            self.workspace.join(path)
        };
        let mut hasher = Sha256::new();
        if let Some(content_hash) = self
            .content_hashes
            .and_then(|hashes| hashes.get(&content_key))
        {
            hasher.update(content_hash.as_bytes());
            hasher.update([1]);
            hasher.update([u8::from(owner_executable(&absolute))]);
        } else if absolute.is_file() {
            let should_hash = self.modified_filepaths.is_empty()
                || self.modified_filepaths.iter().any(|modified| {
                    self.workspace
                        .join(modified)
                        .components()
                        .eq(absolute.components())
                });
            if should_hash {
                hash_file_into(&mut hasher, &absolute)
                    .with_context(|| format!("failed to read source {}", absolute.display()))?;
            }
            hasher.update([1]);
            hasher.update([u8::from(owner_executable(&absolute))]);
        } else {
            if self.modified_filepaths.is_empty() {
                eprintln!("[Warn] File {} not found", absolute.display());
            }
            hasher.update([0]);
        }
        hasher.update(seed);
        hasher.update(label.as_bytes());
        Ok(hasher.finalize().into())
    }

    fn soft_digest(&self, label: &str, seed: &[u8]) -> Result<Option<DigestBytes>> {
        if !is_main_repo(label) {
            return Ok(None);
        }
        let Some((path, _)) = self.resolve(label)? else {
            return Ok(None);
        };
        let absolute = self.workspace.join(path);
        if !absolute.is_file() {
            return Ok(None);
        }
        self.digest(label, seed).map(Some)
    }

    fn resolve(&self, label: &str) -> Result<Option<(PathBuf, String)>> {
        if let Some(path) = main_repo_path(label) {
            return Ok(Some((path.clone(), path.to_string_lossy().into_owned())));
        }
        if let Some(rest) = label.strip_prefix('@') {
            let rest = rest.trim_start_matches('@');
            let Some((repo, target)) = rest.split_once("//") else {
                return Ok(None);
            };
            if !self.fine_grained_external_repos.contains(repo) {
                return Ok(None);
            }
            let relative = PathBuf::from(target.trim_start_matches(':').replace(':', "/"));
            let root = self.external_resolver.resolve(repo)?;
            let key = PathBuf::from("external").join(repo).join(&relative);
            return Ok(Some((
                root.join(relative),
                key.to_string_lossy().into_owned(),
            )));
        }
        Ok(None)
    }
}

fn is_main_repo(label: &str) -> bool {
    label.starts_with("//") || label.starts_with("@//") || label.starts_with("@@//")
}

fn main_repo_path(label: &str) -> Option<PathBuf> {
    let body = label
        .strip_prefix("@@//")
        .or_else(|| label.strip_prefix("@//"))
        .or_else(|| label.strip_prefix("//"))?;
    Some(PathBuf::from(
        body.trim_start_matches(':').replace(':', "/"),
    ))
}

fn owner_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o100 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    fs::metadata(path)
        .map(|metadata| !metadata.permissions().readonly())
        .unwrap_or(false)
}

#[derive(Clone, Default)]
struct ExternalRepoResolver {
    workspace: PathBuf,
    bazel: PathBuf,
    output_base: PathBuf,
    startup_options: Vec<String>,
    no_bazelrc: bool,
    /// Memoizes resolution per apparent repository name. `resolve` is called once
    /// per external source file and can shell out to Bazel, so without this a
    /// repository with N source files costs N Bazel invocations, all of which
    /// serialize on the Bazel server lock.
    resolved: Arc<Mutex<HashMap<String, PathBuf>>>,
    /// Lazily populated apparent -> canonical repository name mapping.
    repo_mapping: Arc<OnceLock<HashMap<String, String>>>,
}

impl ExternalRepoResolver {
    fn new(options: &BazelOptions) -> Result<Self> {
        let mut command = options.command_for_hashing();
        let output = command
            .arg("info")
            .arg("output_base")
            .stdin(Stdio::null())
            .output()
            .context("failed to determine Bazel output_base")?;
        if !output.status.success() {
            bail!(
                "bazel info output_base failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Self {
            workspace: options.workspace.clone(),
            bazel: options.bazel.clone(),
            output_base: PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()),
            startup_options: options.startup_options.clone(),
            no_bazelrc: options.no_bazelrc,
            ..Default::default()
        })
    }

    /// Builds a Bazel command carrying the same startup options `new` used, so
    /// follow-up invocations reach the same server and output base.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.bazel);
        command.current_dir(&self.workspace);
        if self.no_bazelrc {
            command.arg(if cfg!(windows) {
                "--bazelrc=NUL"
            } else {
                "--bazelrc=/dev/null"
            });
        }
        command.args(&self.startup_options);
        command
    }

    fn resolve(&self, repo: &str) -> Result<PathBuf> {
        // The lock is held across resolution so that concurrent hashing threads
        // missing the same repository do not each shell out to Bazel.
        let mut resolved = self
            .resolved
            .lock()
            .map_err(|_| anyhow!("external repository cache was poisoned"))?;
        if let Some(root) = resolved.get(repo) {
            return Ok(root.clone());
        }
        let root = self.resolve_uncached(repo)?;
        resolved.insert(repo.to_owned(), root.clone());
        Ok(root)
    }

    fn resolve_uncached(&self, repo: &str) -> Result<PathBuf> {
        let external = self.output_base.join("external");
        for candidate in [external.join(repo), external.join(format!("{repo}+"))] {
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        // Under Bzlmod the directory is named after the canonical repository name
        // (`rules_req_compile++requirements+pip_deps`), which neither candidate
        // above guesses. `bazel mod dump_repo_mapping` reports that name directly
        // and is one invocation for every repository, so prefer it over the
        // `bazel query` fallback below.
        if let Some(canonical) = self.repo_mapping().get(repo) {
            let candidate = external.join(canonical);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        let output = self
            .command()
            .arg("query")
            .arg(format!("@{repo}//..."))
            .arg("--keep_going")
            .arg("--output")
            .arg("location")
            .stdin(Stdio::null())
            .output()?;
        let line = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(str::to_owned);
        if let Some(line) = line {
            if let Some((location, _)) = line.split_once(": ") {
                let location = PathBuf::from(location);
                if let Ok(relative) = location.strip_prefix(&external) {
                    if let Some(canonical) = relative.components().next() {
                        return Ok(external.join(canonical.as_os_str()));
                    }
                }
            }
        }
        Ok(external.join(repo))
    }

    /// Returns the main repository's apparent -> canonical repository mapping,
    /// fetching it at most once. An empty map means the mapping was unavailable
    /// (for example a WORKSPACE-only build), leaving the `bazel query` fallback.
    fn repo_mapping(&self) -> &HashMap<String, String> {
        self.repo_mapping.get_or_init(|| {
            let output = self
                .command()
                .arg("mod")
                .arg("dump_repo_mapping")
                .arg("")
                .stdin(Stdio::null())
                .output();
            match output {
                Ok(output) if output.status.success() => {
                    parse_apparent_repo_mapping(&String::from_utf8_lossy(&output.stdout))
                }
                _ => HashMap::new(),
            }
        })
    }
}

/// Parses `bazel mod dump_repo_mapping` output, one JSON object of apparent ->
/// canonical repository names per line, into a flat map.
fn parse_apparent_repo_mapping(text: &str) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some(entries) = value.as_object() else {
            continue;
        };
        for (apparent, canonical) in entries {
            let Some(canonical) = canonical.as_str() else {
                continue;
            };
            if !apparent.is_empty() && !canonical.is_empty() {
                mapping.insert(apparent.clone(), canonical.to_owned());
            }
        }
    }
    mapping
}

trait HashingCommand {
    fn command_for_hashing(&self) -> Command;
}

impl HashingCommand for BazelOptions {
    fn command_for_hashing(&self) -> Command {
        let mut command = Command::new(&self.bazel);
        command.current_dir(&self.workspace);
        if self.no_bazelrc {
            command.arg(if cfg!(windows) {
                "--bazelrc=NUL"
            } else {
                "--bazelrc=/dev/null"
            });
        }
        command.args(&self.startup_options);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::blaze_query::{
        attribute, Attribute, ConfiguredRuleInput, GeneratedFile, SourceFile,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn bazel_options(workspace: &Path) -> BazelOptions {
        BazelOptions {
            workspace: workspace.to_path_buf(),
            bazel: PathBuf::from("bazel"),
            startup_options: Vec::new(),
            command_options: Vec::new(),
            cquery_options: Vec::new(),
            use_cquery: false,
            cquery_expression: None,
            keep_going: false,
            fine_grained_external_repos: BTreeSet::new(),
            exclude_external_targets: false,
            exclude_targets_query: None,
            no_bazelrc: true,
            verbose: false,
        }
    }

    fn hash_options(workspace: &Path) -> HashOptions {
        HashOptions {
            bazel: bazel_options(workspace),
            content_hashes: None,
            ignored_attributes: HashSet::new(),
            seed_filepaths: BTreeSet::new(),
            modified_filepaths: BTreeSet::new(),
            track_deps: true,
            always_affected_tags: HashSet::new(),
        }
    }

    fn hash_targets_for_test(
        options: &HashOptions,
        targets: Vec<Target>,
        output_base: &Path,
    ) -> Result<HashFileData> {
        hash_targets_with_environment(
            options,
            targets,
            HashMap::new(),
            ExternalRepoResolver {
                workspace: options.bazel.workspace.clone(),
                bazel: options.bazel.bazel.clone(),
                output_base: output_base.to_path_buf(),
                ..Default::default()
            },
            Some(r#"{"name":"root"}"#.to_owned()),
        )
    }

    fn source_target(name: &str, subincludes: &[&str]) -> Target {
        let mut target = Target::default();
        target.r#type = target::Discriminator::SourceFile;
        target.source_file = buffa::MessageField::some(SourceFile {
            name: name.to_owned(),
            subinclude: subincludes
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            ..Default::default()
        });
        target
    }

    fn rule_target(rule: Rule) -> Target {
        let mut target = Target::default();
        target.r#type = target::Discriminator::Rule;
        target.rule = buffa::MessageField::some(rule);
        target
    }

    fn generated_target(name: &str, generating_rule: &str) -> Target {
        let mut target = Target::default();
        target.r#type = target::Discriminator::GeneratedFile;
        target.generated_file = buffa::MessageField::some(GeneratedFile {
            name: name.to_owned(),
            generating_rule: generating_rule.to_owned(),
            ..Default::default()
        });
        target
    }

    fn string_list_attribute(name: &str, values: &[&str]) -> Attribute {
        Attribute {
            name: name.to_owned(),
            r#type: attribute::Discriminator::StringList,
            string_list_value: values.iter().map(|value| (*value).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn canonical_rule_inputs_are_sorted_and_configuration_sensitive() {
        let rule = Rule {
            configured_rule_input: vec![
                ConfiguredRuleInput {
                    label: Some("//b:b".into()),
                    configuration_checksum: Some("2".into()),
                    ..Default::default()
                },
                ConfiguredRuleInput {
                    label: Some("//a:a".into()),
                    configuration_checksum: Some("1".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            rule_inputs(&rule, true, &BTreeSet::new()),
            [String::from("//a:a|1"), String::from("//b:b|2")]
        );
    }

    #[test]
    fn rule_digest_ignores_generator_location_and_attribute_order() {
        fn attribute(name: &str, value: &str) -> Attribute {
            Attribute {
                name: name.into(),
                r#type: attribute::Discriminator::String,
                string_value: Some(value.into()),
                ..Default::default()
            }
        }
        let mut first = Rule {
            name: "//:x".into(),
            rule_class: "genrule".into(),
            attribute: vec![
                attribute("z", "1"),
                attribute("generator_location", "old"),
                attribute("a", "2"),
            ],
            ..Default::default()
        };
        let mut second = first.clone();
        second.attribute.reverse();
        second.attribute[1].string_value = Some("new".into());
        assert_eq!(
            rule_definition_digest(&first, &HashSet::new()),
            rule_definition_digest(&second, &HashSet::new())
        );
        first.attribute[0].string_value = Some("changed".into());
        assert_ne!(
            rule_definition_digest(&first, &HashSet::new()),
            rule_definition_digest(&second, &HashSet::new())
        );
    }

    #[test]
    fn streamed_rule_preprocessing_preserves_definition_digest() {
        let mut rule = Rule {
            name: "//:x".into(),
            rule_class: "genrule".into(),
            attribute: vec![
                Attribute {
                    name: "tags".into(),
                    r#type: attribute::Discriminator::StringList,
                    string_list_value: vec!["manual".into()],
                    ..Default::default()
                },
                Attribute {
                    name: "cmd".into(),
                    r#type: attribute::Discriminator::String,
                    string_value: Some("echo hi".into()),
                    ..Default::default()
                },
                Attribute {
                    name: "visibility".into(),
                    r#type: attribute::Discriminator::StringList,
                    string_list_value: vec!["//visibility:public".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let expected = rule_definition_digest(&rule, &HashSet::new());

        let actual = prehash_rule(&mut rule, &HashSet::new());

        assert_eq!(actual, expected);
        assert_eq!(
            rule.attribute
                .iter()
                .map(|attribute| attribute.name.as_str())
                .collect::<Vec<_>>(),
            ["tags", "visibility"]
        );
    }

    #[test]
    fn opaque_external_input_maps_to_synthetic_target() {
        assert_eq!(
            transform_external_input("@maven//:guava", &BTreeSet::new()),
            "//external:maven"
        );
    }

    #[test]
    fn absolute_external_instantiation_frames_are_not_main_repo_inputs() {
        let frames = [
            "tools/local.bzl:1:1: local",
            "/tmp/bazel-output/external/rules_java+/java/java_library.bzl:27:18: java_library",
        ];
        let paths = frames
            .iter()
            .filter_map(|frame| frame.split(':').next())
            .filter(|path| {
                (path.ends_with(".bzl") || path.ends_with(".scl"))
                    && !Path::new(path).is_absolute()
                    && !path.starts_with("external/")
                    && !path.starts_with('@')
                    && !path.starts_with("../")
            })
            .collect::<Vec<_>>();
        assert_eq!(paths, ["tools/local.bzl"]);
    }

    #[test]
    fn seed_files_are_resolved_from_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("seed.txt"), "seed contents").unwrap();

        let actual = hash_seed_files(
            workspace.path(),
            &BTreeSet::from([PathBuf::from("seed.txt")]),
        )
        .unwrap();

        let expected: DigestBytes = Sha256::digest(b"seed contents").into();
        assert_eq!(actual, expected);
    }

    #[test]
    fn digest_builder_tracks_direct_transitive_and_dependencies() {
        let mut tracked = TargetDigestBuilder::new(true);
        tracked.direct(b"direct");
        tracked.transitive("//dep:a", b"dependency");
        let tracked = tracked.finish();
        assert_eq!(tracked.deps, ["//dep:a"]);
        assert_ne!(tracked.direct, tracked.overall);

        let mut untracked = TargetDigestBuilder::new(false);
        untracked.direct(b"direct");
        untracked.transitive("//dep:a", b"dependency");
        assert!(untracked.finish().deps.is_empty());
        assert_eq!(digest_hex([0; 32]).len(), 64);
    }

    #[test]
    fn source_helpers_cover_labels_permissions_and_missing_files() {
        assert!(is_main_repo("//pkg:file"));
        assert!(is_main_repo("@//pkg:file"));
        assert!(is_main_repo("@@//pkg:file"));
        assert!(!is_main_repo("@repo//pkg:file"));
        assert_eq!(
            main_repo_path("//pkg:file").unwrap(),
            PathBuf::from("pkg/file")
        );
        assert_eq!(main_repo_path("//:root").unwrap(), PathBuf::from("root"));
        assert!(main_repo_path("not-a-label").is_none());
        assert_eq!(label_package("//pkg:file"), "//pkg");
        assert_eq!(label_package("//pkg"), "//pkg");
        assert!(!owner_executable(Path::new("/definitely/missing/file")));
    }

    #[test]
    fn tags_visibility_and_external_inputs_are_normalized() {
        let rule = Rule {
            attribute: vec![
                string_list_attribute("tags", &["manual", "always"]),
                string_list_attribute(
                    "visibility",
                    &[
                        "//visibility:public",
                        "//pkg:friends",
                        "//pkg:friends",
                        "//pkg:__pkg__",
                        "//other:clients",
                    ],
                ),
            ],
            ..Default::default()
        };
        assert_eq!(rule_tags(&rule), HashSet::from(["manual", "always"]));
        assert_eq!(
            visibility_package_groups(&rule),
            ["//other:clients", "//pkg:friends"]
        );
        assert!(rule_tags(&Rule::default()).is_empty());
        assert!(visibility_package_groups(&Rule::default()).is_empty());

        let fine_grained = BTreeSet::from(["@repo".to_owned()]);
        assert_eq!(
            transform_external_input("@repo//pkg:file", &fine_grained),
            "@repo//pkg:file"
        );
        assert_eq!(
            transform_external_input("@other//pkg:file", &fine_grained),
            "//external:other"
        );
        assert_eq!(
            transform_external_input("@@other+1.0//pkg:file", &fine_grained),
            "//external:other+1.0"
        );
        assert_eq!(
            transform_external_input("@//pkg:file", &fine_grained),
            "@//pkg:file"
        );
    }

    #[test]
    fn source_seed_and_attribute_hashing_are_sensitive_to_inputs() {
        let first = SourceTarget {
            name: "//pkg:a".into(),
            subincludes: vec!["//tools:a.bzl".into()],
        };
        let second = SourceTarget {
            name: "//pkg:b".into(),
            subincludes: vec!["//tools:a.bzl".into()],
        };
        assert_ne!(source_seed(&first), source_seed(&second));

        let attribute = Attribute {
            name: "cmd".into(),
            r#type: attribute::Discriminator::String,
            string_value: Some("echo hi".into()),
            ..Default::default()
        };
        let mut included = Sha256::new();
        hash_attribute(
            &attribute,
            &HashSet::new(),
            &mut SizeCache::new(),
            &mut included,
        );
        let mut ignored = Sha256::new();
        hash_attribute(
            &attribute,
            &HashSet::from(["cmd".to_owned()]),
            &mut SizeCache::new(),
            &mut ignored,
        );
        assert_ne!(included.finalize().to_vec(), ignored.finalize().to_vec());
    }

    #[cfg(unix)]
    #[test]
    fn hashes_rules_sources_generated_files_and_visibility_groups() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        fs::create_dir_all(workspace.path().join("pkg")).unwrap();
        fs::create_dir_all(workspace.path().join("tools")).unwrap();
        fs::write(workspace.path().join("pkg/data.txt"), "data").unwrap();
        fs::write(workspace.path().join("tools/defs.bzl"), "def macro(): pass").unwrap();
        fs::write(workspace.path().join("seed.txt"), "seed").unwrap();
        let mut options = hash_options(workspace.path());
        options.seed_filepaths.insert(PathBuf::from("seed.txt"));

        let group = Rule {
            name: "//pkg:friends".into(),
            rule_class: "package_group".into(),
            attribute: vec![string_list_attribute("packages", &["//client/..."])],
            ..Default::default()
        };
        let dependency = Rule {
            name: "//pkg:dep".into(),
            rule_class: "sh_library".into(),
            rule_input: vec!["//pkg:data.txt".into()],
            ..Default::default()
        };
        let top = Rule {
            name: "//pkg:top".into(),
            rule_class: "genrule".into(),
            attribute: vec![
                string_list_attribute("tags", &["manual"]),
                string_list_attribute("visibility", &["//pkg:friends"]),
            ],
            rule_input: vec![
                "//pkg:dep".into(),
                "//pkg:data.txt".into(),
                "@opaque//lib:file".into(),
            ],
            instantiation_stack: vec!["tools/defs.bzl:1:1: macro".into()],
            ..Default::default()
        };
        let data = hash_targets_for_test(
            &options,
            vec![
                source_target("//pkg:data.txt", &["//tools:defs.bzl"]),
                rule_target(group),
                rule_target(dependency),
                rule_target(top),
                generated_target("//pkg:out.txt", "//pkg:top"),
            ],
            &output_base,
        )
        .unwrap();

        assert_eq!(data.hashes.len(), 5);
        assert_eq!(data.hashes["//pkg:data.txt"].kind, "SourceFile");
        assert_eq!(data.hashes["//pkg:top"].kind, "Rule");
        assert_eq!(data.hashes["//pkg:out.txt"].kind, "GeneratedFile");
        assert!(data.dep_edges["//pkg:top"].contains(&"//pkg:dep".to_owned()));
        assert_eq!(data.dep_edges["//pkg:out.txt"], ["//pkg:top"]);
        assert_eq!(
            data.module_graph_json.as_deref(),
            Some(r#"{"name":"root"}"#)
        );

        fs::write(workspace.path().join("pkg/data.txt"), "changed").unwrap();
        let changed = hash_targets_for_test(
            &options,
            vec![
                source_target("//pkg:data.txt", &["//tools:defs.bzl"]),
                rule_target(Rule {
                    name: "//pkg:dep".into(),
                    rule_class: "sh_library".into(),
                    rule_input: vec!["//pkg:data.txt".into()],
                    ..Default::default()
                }),
            ],
            &output_base,
        )
        .unwrap();
        assert_ne!(
            data.hashes["//pkg:data.txt"].hash,
            changed.hashes["//pkg:data.txt"].hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_bzl_seed_content_hashes_and_modified_filter_affect_sources() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        fs::create_dir_all(workspace.path().join("pkg")).unwrap();
        fs::create_dir_all(workspace.path().join("tools")).unwrap();
        fs::write(workspace.path().join("pkg/data.txt"), "actual").unwrap();
        fs::write(workspace.path().join("tools/defs.bzl"), "v1").unwrap();
        let mut options = hash_options(workspace.path());
        options.content_hashes = Some(BTreeMap::from([(
            "pkg/data.txt".into(),
            "precomputed".into(),
        )]));
        options.modified_filepaths = BTreeSet::from([PathBuf::from("other.txt")]);
        let targets = vec![
            source_target("//pkg:data.txt", &["//tools:defs.bzl"]),
            source_target("//tools:defs.bzl", &[]),
            rule_target(Rule {
                name: "//pkg:rule".into(),
                rule_class: "sh_library".into(),
                rule_input: vec!["//pkg:data.txt".into()],
                ..Default::default()
            }),
        ];
        let first = hash_targets_for_test(&options, targets, &output_base).unwrap();

        fs::write(workspace.path().join("tools/defs.bzl"), "v2").unwrap();
        let second = hash_targets_for_test(
            &options,
            vec![
                source_target("//pkg:data.txt", &["//tools:defs.bzl"]),
                source_target("//tools:defs.bzl", &[]),
                rule_target(Rule {
                    name: "//pkg:rule".into(),
                    rule_class: "sh_library".into(),
                    rule_input: vec!["//pkg:data.txt".into()],
                    ..Default::default()
                }),
            ],
            &output_base,
        )
        .unwrap();
        assert_eq!(
            first.hashes["//pkg:rule"].hash,
            second.hashes["//pkg:rule"].hash
        );

        options.modified_filepaths = BTreeSet::from([PathBuf::from("tools/defs.bzl")]);
        let tracked = hash_targets_for_test(
            &options,
            vec![
                source_target("//pkg:data.txt", &["//tools:defs.bzl"]),
                source_target("//tools:defs.bzl", &[]),
                rule_target(Rule {
                    name: "//pkg:rule".into(),
                    rule_class: "sh_library".into(),
                    rule_input: vec!["//pkg:data.txt".into()],
                    ..Default::default()
                }),
            ],
            &output_base,
        )
        .unwrap();
        assert_ne!(
            first.hashes["//pkg:rule"].hash,
            tracked.hashes["//pkg:rule"].hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn hashing_reports_cycles_and_missing_generators() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let options = hash_options(workspace.path());
        let cycle = hash_targets_for_test(
            &options,
            vec![
                rule_target(Rule {
                    name: "//:a".into(),
                    rule_class: "alias".into(),
                    rule_input: vec!["//:b".into()],
                    ..Default::default()
                }),
                rule_target(Rule {
                    name: "//:b".into(),
                    rule_class: "alias".into(),
                    rule_input: vec!["//:a".into()],
                    ..Default::default()
                }),
            ],
            &output_base,
        )
        .unwrap_err()
        .to_string();
        assert!(cycle.contains("Circular dependency detected"));

        let missing = hash_targets_for_test(
            &options,
            vec![generated_target("//:out", "//:missing")],
            &output_base,
        )
        .unwrap_err()
        .to_string();
        assert!(missing.contains("Unexpected generating rule"));
    }

    #[cfg(unix)]
    #[test]
    fn always_affected_tags_change_each_invocation() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let mut options = hash_options(workspace.path());
        options
            .always_affected_tags
            .insert("non-hermetic".to_owned());
        let target = || {
            rule_target(Rule {
                name: "//:volatile".into(),
                rule_class: "genrule".into(),
                attribute: vec![string_list_attribute("tags", &["non-hermetic"])],
                ..Default::default()
            })
        };
        let first = hash_targets_for_test(&options, vec![target()], &output_base).unwrap();
        let second = hash_targets_for_test(&options, vec![target()], &output_base).unwrap();
        assert_ne!(
            first.hashes["//:volatile"].direct_hash,
            second.hashes["//:volatile"].direct_hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_repo_resolver_handles_existing_repository_directories() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        let external = output_base.join("external");
        fs::create_dir_all(external.join("existing+")).unwrap();
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("bazel"),
            output_base,
            ..Default::default()
        };
        assert_eq!(
            resolver.resolve("existing").unwrap(),
            external.join("existing+")
        );
    }

    /// A stub `bazel` that appends its argv to `log` and prints `stdout`.
    #[cfg(unix)]
    fn stub_bazel(directory: &Path, log: &Path, stdout: &str) -> PathBuf {
        let script = directory.join("stub-bazel");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\ncat <<'STUB_EOF'\n{stdout}\nSTUB_EOF\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn external_repo_resolver_uses_repo_mapping_and_resolves_once() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        let external = output_base.join("external");
        // Only the canonical directory exists, as under Bzlmod.
        let canonical = external.join("rules_req_compile++requirements+pip_deps");
        fs::create_dir_all(&canonical).unwrap();
        let log = workspace.path().join("bazel.log");
        let bazel = stub_bazel(
            workspace.path(),
            &log,
            r#"{"pip_deps": "rules_req_compile++requirements+pip_deps"}"#,
        );
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel,
            output_base,
            ..Default::default()
        };

        for _ in 0..5 {
            assert_eq!(resolver.resolve("pip_deps").unwrap(), canonical);
        }

        // One `mod dump_repo_mapping`, and no `query` fallback, for five lookups.
        let invocations = fs::read_to_string(&log).unwrap();
        assert_eq!(
            invocations.lines().collect::<Vec<_>>(),
            ["mod dump_repo_mapping "]
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_repo_resolver_falls_back_to_query_and_caches_misses() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        let external = output_base.join("external");
        fs::create_dir_all(external.join("queried+1.0")).unwrap();
        let log = workspace.path().join("bazel.log");
        // The mapping has no entry for `queried`, so resolution falls through to
        // `bazel query`; the stub answers both with the same location line.
        let bazel = stub_bazel(
            workspace.path(),
            &log,
            &format!(
                "{}/queried+1.0/pkg/BUILD:1:1: source file @queried//pkg:BUILD",
                external.display()
            ),
        );
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel,
            output_base,
            startup_options: vec!["--max_idle_secs=17".to_owned()],
            ..Default::default()
        };

        for _ in 0..3 {
            assert_eq!(
                resolver.resolve("queried").unwrap(),
                external.join("queried+1.0")
            );
        }

        let invocations = fs::read_to_string(&log).unwrap();
        assert_eq!(
            invocations.lines().collect::<Vec<_>>(),
            [
                "--max_idle_secs=17 mod dump_repo_mapping ",
                "--max_idle_secs=17 query @queried//... --keep_going --output location",
            ]
        );
    }

    #[test]
    fn parse_apparent_repo_mapping_skips_unusable_entries() {
        let mapping = parse_apparent_repo_mapping(
            "not json\n{\"apparent\": \"canonical+1.0\", \"\": \"skipped\", \"empty\": \"\", \"wrong_type\": 7}\n[\"not an object\"]\n",
        );
        assert_eq!(
            mapping,
            HashMap::from([("apparent".to_owned(), "canonical+1.0".to_owned())])
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_hash_tracks_owner_executable_but_ignores_group_and_other_bits() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("path/to/script.sh");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "#!/bin/sh\necho hi\n").unwrap();
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("bazel"),
            output_base: workspace.path().join("output"),
            ..Default::default()
        };
        let modified = BTreeSet::new();
        let hasher = SourceHasher {
            workspace: workspace.path(),
            content_hashes: None,
            modified_filepaths: &modified,
            fine_grained_external_repos: BTreeSet::new(),
            external_resolver: resolver,
        };
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&file, permissions.clone()).unwrap();
        let baseline = hasher.digest("//path/to:script.sh", b"seed").unwrap();

        permissions.set_mode(0o655);
        fs::set_permissions(&file, permissions.clone()).unwrap();
        assert_eq!(
            hasher.digest("//path/to:script.sh", b"seed").unwrap(),
            baseline
        );

        permissions.set_mode(0o744);
        fs::set_permissions(&file, permissions).unwrap();
        assert_ne!(
            hasher.digest("//path/to:script.sh", b"seed").unwrap(),
            baseline
        );
    }

    #[test]
    fn source_hash_distinguishes_empty_missing_and_unrecognized_labels() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("path/to/empty.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "").unwrap();
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("bazel"),
            output_base: workspace.path().join("output"),
            ..Default::default()
        };
        let modified = BTreeSet::new();
        let hasher = SourceHasher {
            workspace: workspace.path(),
            content_hashes: None,
            modified_filepaths: &modified,
            fine_grained_external_repos: BTreeSet::new(),
            external_resolver: resolver,
        };
        let existing = hasher.digest("//path/to:empty.txt", b"seed").unwrap();
        fs::remove_file(&file).unwrap();
        let missing = hasher.digest("//path/to:empty.txt", b"seed").unwrap();
        assert_ne!(existing, missing);
        assert_eq!(
            hasher.digest("@not_a_valid_label", b"seed").unwrap(),
            <DigestBytes>::from(Sha256::digest([]))
        );
        assert_eq!(
            hasher.digest("not-a-bazel-label", b"seed").unwrap(),
            <DigestBytes>::from(Sha256::digest([]))
        );
        assert!(hasher
            .soft_digest("//path/to:empty.txt", b"seed")
            .unwrap()
            .is_none());
        assert!(hasher
            .soft_digest("@repo//:file", b"seed")
            .unwrap()
            .is_none());
    }

    #[test]
    fn source_hash_accepts_main_repo_label_forms_and_content_hash_keys() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("path/to/file.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "on disk").unwrap();
        let content_hashes = BTreeMap::from([
            ("path/to/file.txt".to_owned(), "provided".to_owned()),
            (
                "external/ext/path/to/file.txt".to_owned(),
                "external-provided".to_owned(),
            ),
        ]);
        let external_file = workspace
            .path()
            .join("output/external/ext/path/to/file.txt");
        fs::create_dir_all(external_file.parent().unwrap()).unwrap();
        fs::write(&external_file, "external disk").unwrap();
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("bazel"),
            output_base: workspace.path().join("output"),
            ..Default::default()
        };
        let modified = BTreeSet::new();
        let hasher = SourceHasher {
            workspace: workspace.path(),
            content_hashes: Some(&content_hashes),
            modified_filepaths: &modified,
            fine_grained_external_repos: BTreeSet::from(["ext".to_owned()]),
            external_resolver: resolver,
        };
        let plain = hasher.digest("//path/to:file.txt", b"seed").unwrap();
        assert_ne!(
            hasher.digest("@//path/to:file.txt", b"seed").unwrap(),
            plain
        );
        assert_ne!(
            hasher.digest("@@//path/to:file.txt", b"seed").unwrap(),
            plain
        );
        assert_ne!(
            hasher.digest("//:path/to/file.txt", b"seed").unwrap(),
            <DigestBytes>::from(Sha256::digest([]))
        );
        assert_ne!(
            hasher.digest("@ext//path/to:file.txt", b"seed").unwrap(),
            <DigestBytes>::from(Sha256::digest([]))
        );

        let not_fine_grained = SourceHasher {
            workspace: workspace.path(),
            content_hashes: Some(&content_hashes),
            modified_filepaths: &modified,
            fine_grained_external_repos: BTreeSet::new(),
            external_resolver: hasher.external_resolver.clone(),
        };
        assert_eq!(
            not_fine_grained
                .digest("@ext//path/to:file.txt", b"seed")
                .unwrap(),
            <DigestBytes>::from(Sha256::digest([]))
        );
    }

    #[test]
    fn soft_digest_rejects_directories() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join("path/to/dir")).unwrap();
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("bazel"),
            output_base: workspace.path().join("output"),
            ..Default::default()
        };
        let modified = BTreeSet::new();
        let hasher = SourceHasher {
            workspace: workspace.path(),
            content_hashes: None,
            modified_filepaths: &modified,
            fine_grained_external_repos: BTreeSet::new(),
            external_resolver: resolver,
        };
        assert!(hasher
            .soft_digest("//path/to:dir", b"seed")
            .unwrap()
            .is_none());
    }
}
