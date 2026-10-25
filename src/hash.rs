use crate::bazel::{
    encode_configured_input, split_configured, target_label, BazelOptions, QueriedTargets,
    RepositoryInputGroup, RepositoryInputs,
};
use crate::cancel::{self, Cancellation};
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
use std::time::Instant;

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
    // The traversal cache and the output share these labels until all parents have been hashed.
    deps: Option<Rc<Vec<String>>>,
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
            deps: (!self.deps.is_empty()).then(|| Rc::new(self.deps)),
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

impl HashTarget {
    fn label(&self) -> &str {
        match self {
            HashTarget::Rule(label) | HashTarget::Source(label) => label,
            HashTarget::Generated { name, .. } => name,
        }
    }
}

struct SourceTarget {
    name: String,
    subincludes: Vec<String>,
}

/// Wall-clock breakdown of one hash generation, in pipeline order: the `bazel query` for all
/// targets (including proto parsing), obtaining the module graph for metadata, and seeding plus
/// hashing every source file and target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GenerationTimings {
    pub bazel_query_millis: u64,
    pub module_graph_millis: u64,
    pub target_hash_millis: u64,
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

pub fn generate_hashes(options: &HashOptions) -> Result<HashFileData> {
    generate_hashes_timed(options).map(|(data, _)| data)
}

pub fn generate_hashes_timed(options: &HashOptions) -> Result<(HashFileData, GenerationTimings)> {
    let mut definition_digests = HashMap::new();
    let query_started = Instant::now();
    let QueriedTargets {
        targets,
        module_graph_json,
        root_mapping,
    } = options
        .bazel
        .query_all_targets_with(|target, shared_inputs| {
            let Some(rule) = target.rule.as_option_mut() else {
                return;
            };
            definition_digests.insert(
                rule.name.clone(),
                PreparedRule {
                    definition_digest: prehash_rule(rule, &options.ignored_attributes),
                    shared_inputs,
                },
            );
        })?;
    let bazel_query_millis = elapsed_millis(query_started);
    // The query is the phase worth cancelling; a request abandoned during it stops here instead
    // of hashing targets nobody will read.
    options.bazel.check_cancelled()?;
    let (data, mut timings) = hash_targets_with_digests(
        options,
        targets,
        definition_digests,
        module_graph_json,
        root_mapping,
    )?;
    timings.bazel_query_millis = bazel_query_millis;
    Ok((data, timings))
}

pub fn hash_targets(options: &HashOptions, targets: Vec<Target>) -> Result<HashFileData> {
    hash_targets_with_digests(options, targets, HashMap::new(), None, None).map(|(data, _)| data)
}

fn hash_targets_with_digests(
    options: &HashOptions,
    targets: Vec<Target>,
    definition_digests: HashMap<String, PreparedRule>,
    module_graph_json: Option<String>,
    root_mapping: Option<Vec<u8>>,
) -> Result<(HashFileData, GenerationTimings)> {
    let hash_started = Instant::now();
    let external_resolver = ExternalRepoResolver::new(&options.bazel, root_mapping)?;
    let module_graph_started = Instant::now();
    let module_graph_json = module_graph_json.or_else(|| options.bazel.module_graph_json());
    options.bazel.check_cancelled()?;
    let module_graph_millis = elapsed_millis(module_graph_started);
    let data = hash_targets_with_environment(
        options,
        targets,
        definition_digests,
        external_resolver,
        module_graph_json,
    )?;
    let timings = GenerationTimings {
        bazel_query_millis: 0,
        module_graph_millis,
        target_hash_millis: elapsed_millis(hash_started).saturating_sub(module_graph_millis),
    };
    Ok((data, timings))
}

fn hash_targets_with_environment(
    options: &HashOptions,
    targets: Vec<Target>,
    mut definition_digests: HashMap<String, PreparedRule>,
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

    let mut rules = HashMap::<String, Rc<RuleNode>>::with_capacity(definition_digests.len());
    let mut decoded_rules = Vec::<(String, Rule)>::new();
    let mut source_targets = Vec::new();
    let mut hash_targets = Vec::with_capacity(targets.len());
    for target in targets {
        match target.r#type {
            target::Discriminator::Rule => {
                if let Some(rule) = target.rule.into_option() {
                    hash_targets.push(HashTarget::Rule(rule.name.clone()));
                    // Generated targets are prehashed and deduplicated; raw callers provide no
                    // digests and retain the ordered fallback below, including duplicate names.
                    match definition_digests.remove_entry(&rule.name) {
                        Some((name, prepared)) => {
                            rules.insert(name, Rc::new(RuleNode { rule, prepared }));
                        }
                        None => decoded_rules.push((rule.name.clone(), rule)),
                    }
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
            .map(|(name, rule)| {
                let definition_digest = rule_definition_digest(&rule, &options.ignored_attributes);
                (
                    name,
                    RuleNode {
                        rule,
                        prepared: PreparedRule {
                            definition_digest,
                            shared_inputs: None,
                        },
                    },
                )
            })
            .collect::<Vec<_>>()
    });
    rules.reserve(processed_rules.len());
    for (name, node) in processed_rules {
        rules.insert(name, Rc::new(node));
    }
    let mut label_instances = None;
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
            .or_else(|| {
                label_instances
                    .get_or_insert_with(|| configured_label_instances(rules.keys()))
                    .get(target_label(generating_rule))
                    .and_then(|keys| keys.first())
                    .and_then(|key| rules.get(key))
            })
            .cloned()
            .ok_or_else(|| anyhow!("Unexpected generating rule {generating_rule}"))?;
        rules.insert(name.clone(), rule);
        if let (Some(index), (label, Some(_))) = (&mut label_instances, split_configured(name)) {
            let instances = index.entry(label.to_owned()).or_default();
            if let Err(position) = instances.binary_search(name) {
                instances.insert(position, name.clone());
            }
        }
    }

    let mut context = HashContext {
        options,
        source_hasher: &source_hasher,
        source_digests,
        rules,
        label_instances,
        cache: HashMap::new(),
        repository_input_digests: HashMap::new(),
        seed_hash,
        package_bzl_seeds,
        bzl_digests: HashMap::new(),
        always_affected_seed: unique_invocation_seed(),
        cycle_warnings: HashSet::new(),
    };
    // `--order_output=no` makes the query order arbitrary. Acyclic hashes do not care, but the
    // digest of a cycle depends on which member is entered first, so fix the traversal order.
    hash_targets.sort_by(|left, right| left.label().cmp(right.label()));
    let mut digests = Vec::with_capacity(hash_targets.len());
    for target in hash_targets {
        let (key, kind, digest) = match target {
            HashTarget::Rule(key) => {
                let digest = context.rule_digest(&key, &mut Vec::new())?;
                (key, "Rule", digest)
            }
            HashTarget::Generated {
                name,
                generating_rule,
            } => {
                let mut digest = context.rule_digest(&generating_rule, &mut Vec::new())?;
                digest.deps = options.track_deps.then(|| Rc::new(vec![generating_rule]));
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
                        deps: None,
                    },
                )
            }
        };
        digests.push((key, kind, digest));
    }
    drop(context);
    let (hashes, dep_edges) = aggregate_by_label(digests, options.track_deps);
    Ok(HashFileData {
        hashes,
        module_graph_json,
        dependency_fingerprint: None,
        dep_edges,
    })
}

/// Groups the keys that carry a configuration checksum by label, each group sorted.
fn configured_label_instances<'a>(
    keys: impl Iterator<Item = &'a String>,
) -> HashMap<String, Vec<String>> {
    let mut instances = HashMap::<String, Vec<String>>::new();
    for key in keys {
        if let (label, Some(_)) = split_configured(key) {
            instances
                .entry(label.to_owned())
                .or_default()
                .push(key.clone());
        }
    }
    for keys in instances.values_mut() {
        keys.sort_unstable();
    }
    instances
}

/// Folds per-key digests into the per-label hashes the hash file is keyed by.
///
/// A label hashed once (every target of a plain query, and most of a cquery) keeps its
/// digest as is, so hash files do not change where nothing was collapsed. A label with
/// several configured instances hashes all of them, in key order, so a change reaching any
/// instance changes the label, and its dependency edges are the union of its instances'.
fn aggregate_by_label(
    digests: Vec<(String, &'static str, TargetDigest)>,
    track_deps: bool,
) -> (BTreeMap<String, TargetHash>, BTreeMap<String, Vec<String>>) {
    // Hash traversal visits full keys in order. If the resulting labels are strictly
    // increasing, each has one instance and both output maps can be built in order.
    if digests.is_sorted_by(|a, b| target_label(&a.0) < target_label(&b.0)) {
        let mut instances = Vec::with_capacity(1);
        let mut edges = Vec::with_capacity(if track_deps { digests.len() } else { 0 });
        let hashes = digests
            .into_iter()
            .map(|instance| {
                instances.push(instance);
                let (label, hash, deps) = label_hash(&mut instances);
                if track_deps {
                    edges.push((label.clone(), deps));
                }
                (label, hash)
            })
            .collect();
        return (hashes, edges.into_iter().collect());
    }
    let mut digests = digests
        .into_iter()
        .map(|(key, kind, digest)| (target_label(&key).len(), key, kind, digest))
        .collect::<Vec<_>>();
    digests.sort_by(|a, b| a.1[..a.0].cmp(&b.1[..b.0]).then_with(|| a.1.cmp(&b.1)));
    let mut hashes = BTreeMap::new();
    let mut dep_edges = BTreeMap::new();
    let mut insert_label = |instances: &mut Vec<_>| {
        let (label, hash, deps) = label_hash(instances);
        if track_deps {
            dep_edges.insert(label.clone(), deps);
        }
        hashes.insert(label, hash);
    };
    let mut instances = Vec::<(String, &'static str, TargetDigest)>::new();
    for (label_length, key, kind, digest) in digests {
        if instances
            .last()
            .is_some_and(|last| target_label(&last.0) != &key[..label_length])
        {
            insert_label(&mut instances);
        }
        instances.push((key, kind, digest));
    }
    if !instances.is_empty() {
        insert_label(&mut instances);
    }
    (hashes, dep_edges)
}

fn label_hash(
    instances: &mut Vec<(String, &'static str, TargetDigest)>,
) -> (String, TargetHash, Vec<String>) {
    let mut label = std::mem::take(&mut instances[0].0);
    label.truncate(target_label(&label).len());
    let kind = instances[0].1;
    let (overall, direct) = if let [(_, _, digest)] = instances.as_slice() {
        (digest.overall, digest.direct)
    } else {
        let mut overall = Sha256::new();
        let mut direct = Sha256::new();
        for (_, _, digest) in instances.iter() {
            overall.update(digest.overall);
            direct.update(digest.direct);
        }
        (overall.finalize().into(), direct.finalize().into())
    };
    let mut edge_lists = instances
        .drain(..)
        .filter_map(|(_, _, digest)| digest.deps)
        .map(Rc::unwrap_or_clone);
    let mut deps = edge_lists.next().unwrap_or_default();
    for mut edges in edge_lists {
        deps.append(&mut edges);
    }
    for dependency in &mut deps {
        dependency.truncate(target_label(dependency).len());
    }
    // Plain-query rule inputs are normally already unique and in label order.
    if !deps.is_sorted_by(|left, right| left < right) {
        let mut seen = HashSet::new();
        deps.retain(|dependency| seen.insert(dependency.clone()));
    }
    (
        label,
        TargetHash {
            kind: kind.to_owned(),
            hash: overall.into(),
            direct_hash: direct.into(),
        },
        deps,
    )
}

struct HashContext<'a> {
    options: &'a HashOptions,
    source_hasher: &'a SourceHasher<'a>,
    source_digests: HashMap<String, DigestBytes>,
    rules: HashMap<String, Rc<RuleNode>>,
    /// Keys in `rules` grouped by label, sorted, for configured keys only: the fallback when
    /// an input names a configured instance that is not in `rules`. Built on the first miss,
    /// since most graphs never need it, and never for a plain query, where every key is a
    /// bare label and resolves directly.
    label_instances: Option<HashMap<String, Vec<String>>>,
    cache: HashMap<String, TargetDigest>,
    repository_input_digests: HashMap<String, (Vec<u8>, Sha256)>,
    seed_hash: DigestBytes,
    package_bzl_seeds: BTreeMap<String, DigestBytes>,
    bzl_digests: HashMap<String, Option<DigestBytes>>,
    always_affected_seed: DigestBytes,
    /// Targets already reported as the entry of a cycle, so each is warned about once.
    cycle_warnings: HashSet<String>,
}

impl HashContext<'_> {
    fn apply_repository_inputs(
        &mut self,
        inputs: &RepositoryInputGroup,
        builder: &mut TargetDigestBuilder,
    ) -> bool {
        use std::collections::hash_map::Entry;
        let (labels, transitive) = match self
            .repository_input_digests
            .entry(inputs.extension.clone())
        {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let mut labels = Vec::new();
                let mut transitive = Sha256::new();
                for label in &inputs.labels {
                    // An unfinished rule may enter a cycle; let the normal traversal decide
                    // its digest. A source contributes to the owner's direct hash instead.
                    let Some(digest) = self.cache.get(label) else {
                        return false;
                    };
                    if self.source_digests.contains_key(label) {
                        return false;
                    }
                    labels.extend_from_slice(label.as_bytes());
                    transitive.update(digest.overall);
                }
                entry.insert((labels, transitive))
            }
        };
        // Each hub has its own direct prefix. Its ordered child digests form the
        // entire transitive prefix; visibility edges, if present, are appended later.
        builder.direct(labels);
        builder.overall = transitive.clone();
        if builder.track_deps {
            builder.deps.clone_from(&inputs.labels);
        }
        true
    }

    fn cached_rule_digest(&self, key: &str) -> Option<&TargetDigest> {
        self.cache.get(key).or_else(|| {
            self.rules
                .get(key)
                .and_then(|rule| self.cache.get(&rule.rule.name))
        })
    }

    fn rule_overall_digest(&mut self, key: &str, path: &mut Vec<String>) -> Result<DigestBytes> {
        // Parents do not need a cached dependency vector when propagating a child's digest.
        if let Some(digest) = self.cached_rule_digest(key) {
            return Ok(digest.overall);
        }
        self.rule_digest(key, path).map(|digest| digest.overall)
    }

    /// Digests one rule, named by its key: its label, or under cquery its configured identity
    /// `label|checksum` (see [`crate::bazel::CONFIGURED_SEPARATOR`]). Two configured instances
    /// of one label are two nodes here, each with its own inputs, so an edge from one instance
    /// to the other is a dependency and not a cycle.
    fn rule_digest(&mut self, key: &str, path: &mut Vec<String>) -> Result<TargetDigest> {
        if let Some(digest) = self.cached_rule_digest(key) {
            return Ok(digest.clone());
        }
        let rule = self
            .rules
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("Unable to find rule {key}"))?;
        let canonical_key = rule.rule.name.clone();
        if let Some(index) = path.iter().position(|entry| entry == &canonical_key) {
            // A back edge: the query graph can hold cycles a build never sees (rules_go's
            // `go_context_data` and the nogo analyzers reference each other through non-dependency
            // attributes). The target is still being hashed, so stand in with a digest of its own
            // definition. Which member of the cycle is entered first is fixed by the sorted
            // traversal in `hash_targets_with_environment`, so the result is deterministic.
            if self.cycle_warnings.insert(canonical_key.clone()) {
                let cycle = path[index..]
                    .iter()
                    .chain(std::iter::once(&canonical_key))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" -> ");
                eprintln!(
                    "[Warn] Circular dependency detected: {cycle}; the back edge to {canonical_key} is hashed from its own definition only"
                );
            }
            let mut hasher = Sha256::new();
            hasher.update(b"cycle");
            hasher.update(rule.prepared.definition_digest);
            let placeholder: DigestBytes = hasher.finalize().into();
            return Ok(TargetDigest {
                overall: placeholder,
                direct: placeholder,
                deps: None,
            });
        }
        path.push(canonical_key.clone());

        let mut builder = TargetDigestBuilder::new(self.options.track_deps);
        builder.direct(rule.prepared.definition_digest);
        builder.direct(self.seed_hash);
        if let Some(seed) = self.rule_bzl_seed(&rule.rule)? {
            builder.direct(seed);
        } else if let Some(seed) = self
            .package_bzl_seeds
            .get(label_package(target_label(&rule.rule.name)))
        {
            builder.direct(seed);
        }
        if !self.options.always_affected_tags.is_empty()
            && rule_tags(&rule.rule)
                .iter()
                .any(|tag| self.options.always_affected_tags.contains(*tag))
        {
            builder.direct(self.always_affected_seed);
        }

        // cquery prints canonical labels (`@@rules_rs++crate+bd_crates//:anyhow`), while the
        // synthetic `//external:<repo>` nodes are named by the root's apparent names. Only fetch
        // the translation when a rule actually names a canonical repository, or a fine-grained
        // repository is given by its canonical name and has to be recognized under the apparent
        // name `query` prints, so a plain `query` or WORKSPACE run never pays for the extra
        // `bazel mod` call.
        let empty_repo_nodes = HashMap::new();
        let canonical_repo_nodes = if rule
            .rule
            .rule_input
            .iter()
            .any(|input| is_canonical_external_label(input))
            || self
                .options
                .bazel
                .fine_grained_external_repos
                .iter()
                .any(|repo| repo.starts_with("@@"))
        {
            self.source_hasher.external_resolver.canonical_repo_nodes()
        } else {
            &empty_repo_nodes
        };
        let mut inputs = rule_inputs(
            &rule.rule,
            self.options.bazel.use_cquery,
            &self.options.bazel.fine_grained_external_repos,
            canonical_repo_nodes,
        );
        if let Some(group) = &rule.prepared.shared_inputs {
            if !self.apply_repository_inputs(group, &mut builder) {
                inputs.extend(
                    group
                        .labels
                        .iter()
                        .map(|input| Cow::Borrowed(input.as_str())),
                );
            }
        }
        for encoded_input in inputs {
            builder.direct(encoded_input.as_bytes());
            let input_label = target_label(&encoded_input);
            if let Some(source_digest) = self.source_digests.get(input_label) {
                builder.direct(source_digest);
            } else if let Some(dependency) = self
                .resolve_dependency(&encoded_input, input_label, path)
                .filter(|dependency| dependency != &canonical_key)
            {
                let digest = self.rule_overall_digest(&dependency, path)?;
                builder.transitive(&dependency, digest);
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
                if package_group == target_label(&canonical_key) {
                    continue;
                }
                let Some(group) = self.rules.get(package_group) else {
                    continue;
                };
                if group.rule.rule_class != "package_group" {
                    continue;
                }
                builder.direct(package_group.as_bytes());
                let digest = self.rule_overall_digest(package_group, path)?;
                builder.transitive(package_group, digest);
            }
        }

        path.pop();
        let digest = builder.finish();
        self.cache.insert(canonical_key, digest.clone());
        Ok(digest)
    }

    /// The key of the rule a rule input resolves to.
    ///
    /// A configured input names the exact instance it points at; that instance is used when
    /// it was hashed, and a bare label (every input of a plain query) resolves directly.
    /// Otherwise the edge falls back to a configured instance of the same label, which keeps
    /// edges to instances outside the query result resolving as they always have. The
    /// fallback never closes a cycle through the path being hashed: a configured graph Bazel
    /// analyzed has none, so such an edge points at an instance that is simply not in the
    /// result, and is dropped.
    fn resolve_dependency<'a>(
        &mut self,
        encoded_input: &'a str,
        input_label: &'a str,
        path: &[String],
    ) -> Option<Cow<'a, str>> {
        if self.rules.contains_key(encoded_input) {
            return Some(Cow::Borrowed(encoded_input));
        }
        if self.rules.contains_key(input_label) {
            return Some(Cow::Borrowed(input_label));
        }
        let rules = &self.rules;
        let fallback = self
            .label_instances
            .get_or_insert_with(|| configured_label_instances(rules.keys()))
            .get(input_label)?
            .first()?;
        if path.iter().any(|entry| entry == fallback) {
            return None;
        }
        Some(Cow::Owned(fallback.clone()))
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
    prepared: PreparedRule,
}

struct PreparedRule {
    definition_digest: DigestBytes,
    shared_inputs: Option<RepositoryInputs>,
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

/// Hashes a directory tree deterministically: every entry's workspace-independent relative path
/// and kind, plus regular-file contents and executable bits and symlink targets. Entries are
/// visited in sorted order so the digest does not depend on readdir order.
pub(crate) fn hash_directory_into(hasher: &mut Sha256, root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(root)?
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            let target = fs::read_link(entry.path())?;
            hasher.update(b"symlink");
            hasher.update(target.to_string_lossy().as_bytes());
        } else if file_type.is_dir() {
            hasher.update(b"dir");
        } else if file_type.is_file() {
            let mut content = Sha256::new();
            hash_file_into(&mut content, entry.path())?;
            hasher.update(b"file");
            hasher.update(content.finalize());
            hasher.update([u8::from(owner_executable(entry.path()))]);
        } else {
            hasher.update(b"other");
        }
        hasher.update([0]);
    }
    Ok(())
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

/// Repository rules (`//external:*`) that pin their download with `sha256` or `integrity` fetch
/// byte-identical content from any URL, so a mirror or registry move must not impact every
/// target that depends on the repository. Only such rules get their `url`/`urls` skipped.
fn pins_download_content(rule: &Rule) -> bool {
    rule.name.starts_with("//external:")
        && rule.attribute.iter().any(|attribute| {
            matches!(attribute.name.as_str(), "sha256" | "integrity")
                && attribute
                    .string_value
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
        })
}

/// Registry patches, overlay files and the module's `MODULE.bazel` each carry their own integrity
/// (the `remote_patches` values, `remote_file_integrity`, `remote_module_file_integrity`), so,
/// like a pinned `url`/`urls`, their URLs cannot change what is fetched. A
/// `file://%workspace%/...` registry expands to the checkout's absolute path, so hashing these
/// URLs would give identical checkouts at different paths different hashes. Returns a copy of
/// `attribute` without the URLs that an integrity in `repository_attributes` pins, or `None` when
/// `attribute` is none of `remote_patches`, `remote_file_urls` and `remote_module_file_urls`.
pub(crate) fn without_integrity_pinned_urls(
    attribute: &crate::proto::blaze_query::Attribute,
    repository_attributes: &[crate::proto::blaze_query::Attribute],
) -> Option<crate::proto::blaze_query::Attribute> {
    if !matches!(
        attribute.name.as_str(),
        "remote_patches" | "remote_file_urls" | "remote_module_file_urls"
    ) {
        return None;
    }
    let mut stripped = attribute.clone();
    match attribute.name.as_str() {
        "remote_patches" => {
            // Entries keep their order, which is the order the patches apply in.
            for entry in &mut stripped.string_dict_value {
                if !entry.value.is_empty() {
                    entry.key.clear();
                }
            }
        }
        "remote_module_file_urls" => {
            let pinned = repository_attributes.iter().any(|other| {
                other.name == "remote_module_file_integrity"
                    && other
                        .string_value
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
            });
            if pinned {
                stripped.string_list_value.clear();
            }
        }
        "remote_file_urls" => {
            let pinned = repository_attributes
                .iter()
                .filter(|other| other.name == "remote_file_integrity")
                .flat_map(|other| &other.string_dict_value)
                .filter(|entry| !entry.value.is_empty())
                .map(|entry| entry.key.as_str())
                .collect::<HashSet<_>>();
            for entry in &mut stripped.string_list_dict_value {
                if pinned.contains(entry.key.as_str()) {
                    entry.value.clear();
                }
            }
        }
        _ => return None,
    }
    Some(stripped)
}

fn rule_definition_digest(rule: &Rule, ignored: &HashSet<String>) -> DigestBytes {
    let mut hasher = Sha256::new();
    hasher.update(rule.rule_class.as_bytes());
    hasher.update(target_label(&rule.name).as_bytes());
    if let Some(environment_hash) = &rule.skylark_environment_hash_code {
        hasher.update(environment_hash.as_bytes());
    }
    let skip_urls = pins_download_content(rule);
    let repository_attributes = rule
        .name
        .starts_with("//external:")
        .then_some(rule.attribute.as_slice());
    let mut size_cache = SizeCache::new();
    let attributes_are_sorted = rule
        .attribute
        .windows(2)
        .all(|pair| pair[0].name <= pair[1].name);
    if attributes_are_sorted {
        for attribute in &rule.attribute {
            hash_attribute(
                attribute,
                ignored,
                skip_urls,
                repository_attributes,
                &mut size_cache,
                &mut hasher,
            );
        }
    } else {
        let mut attributes = rule.attribute.iter().collect::<Vec<_>>();
        attributes.sort_by_key(|attribute| &attribute.name);
        for attribute in attributes {
            hash_attribute(
                attribute,
                ignored,
                skip_urls,
                repository_attributes,
                &mut size_cache,
                &mut hasher,
            );
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
    skip_urls: bool,
    repository_attributes: Option<&[crate::proto::blaze_query::Attribute]>,
    size_cache: &mut SizeCache,
    hasher: &mut Sha256,
) {
    if attribute.name == "generator_location" || ignored.contains(&attribute.name) {
        return;
    }
    if skip_urls && matches!(attribute.name.as_str(), "url" | "urls") {
        return;
    }
    if let Some(stripped) = repository_attributes
        .and_then(|attributes| without_integrity_pinned_urls(attribute, attributes))
    {
        stripped.encode_with_cache(size_cache, &mut Sha256Sink(hasher));
        return;
    }
    attribute.encode_with_cache(size_cache, &mut Sha256Sink(hasher));
}

fn rule_inputs<'a>(
    rule: &'a Rule,
    use_cquery: bool,
    fine_grained_repos: &BTreeSet<String>,
    canonical_repo_nodes: &HashMap<String, String>,
) -> Vec<Cow<'a, str>> {
    let external_synthetic = rule
        .rule_input
        .iter()
        // Query already includes these inputs below; cquery needs the synthetic copy.
        .filter(|input| use_cquery || !input.starts_with("//external:"))
        .map(|input| transform_external_input(input, fine_grained_repos, canonical_repo_nodes))
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

/// Whether `label` names a target in an external repository by canonical name (`@@repo//...`),
/// as `bazel cquery` prints every label. `@@//pkg:t` is the main repository.
fn is_canonical_external_label(label: &str) -> bool {
    label.starts_with("@@") && !label.starts_with("@@//")
}

/// Rewrites an input label in an external repository to the synthetic `//external:<repo>` node
/// `bazel::lower_repositories` produces for that repository, unless the repository is hashed
/// fine-grained, in which case the label is kept and resolved target by target.
///
/// Nodes are named by the apparent repository name the root module sees (`//external:bd_crates`),
/// falling back to the canonical name for modules the root cannot see (`//external:rules_cc+`,
/// #510). `bazel query` prints apparent labels, so the repository name is the node name; `bazel
/// cquery` prints canonical labels (`@@rules_rs++crate+bd_crates//:anyhow`), which
/// `canonical_repo_nodes` (canonical -> node name) translates. Without that translation every
/// cquery-mode edge to a repository node was silently dropped, so a lock file or patch hashed into
/// the node never reached any main-repository target.
fn transform_external_input<'a>(
    input: &'a str,
    fine_grained_repos: &BTreeSet<String>,
    canonical_repo_nodes: &HashMap<String, String>,
) -> Cow<'a, str> {
    if is_main_repo(input) || !input.starts_with('@') {
        return Cow::Borrowed(input);
    }
    let Some((repo, _)) = input.split_once("//") else {
        return Cow::Borrowed(input);
    };
    let name = repo.trim_start_matches('@');
    let node = if repo.starts_with("@@") {
        canonical_repo_nodes.get(name).map_or(name, String::as_str)
    } else {
        name
    };
    // Match on the full repo name, not a bare string prefix: with hub-and-spoke repos (e.g.
    // rules_python's `@pip` hub and `@pip_<pkg>` spokes), a bare prefix check makes every spoke
    // label look like it belongs to the fine-grained hub, so it is never rewritten to its
    // `//external:<spoke>` seed and changes stop propagating. Users may name the repository by
    // either its apparent name (`@pip`) or, in cquery mode, its canonical name
    // (`@@rules_python++pip+pip`).
    if names_fine_grained_repo(name, node, fine_grained_repos, canonical_repo_nodes) {
        return Cow::Borrowed(input);
    }
    Cow::Owned(format!("//external:{node}"))
}

/// Whether a label's repository -- `name` as the label spells it, `node` its `//external:` node
/// name -- is one the user fine-grained, by its apparent name (`@crates`) or its canonical name
/// (`@@rules_rs++crate+crates`). Rule inputs and source files share this, so a repository
/// fine-grained under either name is treated the same way when it is printed under the other.
fn names_fine_grained_repo(
    name: &str,
    node: &str,
    fine_grained_repos: &BTreeSet<String>,
    canonical_repo_nodes: &HashMap<String, String>,
) -> bool {
    fine_grained_repos.iter().any(|repo| {
        let repo = repo.trim_start_matches('@');
        repo == name
            || repo == node
            || canonical_repo_nodes
                .get(repo)
                .is_some_and(|repo_node| repo_node == node)
    })
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
        } else if absolute.is_dir() {
            // A label can point at a directory (`srcs = ["testdata"]`). Bazel only tracks the
            // directory itself, but its contents are what the consuming action reads, so hash the
            // tree: otherwise an edit inside it never impacts anything.
            let should_hash = self.modified_filepaths.is_empty()
                || self
                    .modified_filepaths
                    .iter()
                    .any(|modified| self.workspace.join(modified).starts_with(&absolute));
            if should_hash {
                hash_directory_into(&mut hasher, &absolute).with_context(|| {
                    format!("failed to read source directory {}", absolute.display())
                })?;
            }
            hasher.update([2]);
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
            let canonical = rest.starts_with('@');
            let rest = rest.trim_start_matches('@');
            let Some((repo, target)) = rest.split_once("//") else {
                return Ok(None);
            };
            // The same matching `transform_external_input` applies to the rule's inputs: a label
            // printed under either name (`@crates//...` by `query`, `@@rules_rs++crate+crates//...`
            // by `cquery`) is fine-grained when the user named the repository either way.
            let fine_grained = self.fine_grained_external_repos.contains(repo)
                || (!self.fine_grained_external_repos.is_empty() && {
                    let nodes = self.external_resolver.canonical_repo_nodes();
                    let node = if canonical {
                        nodes.get(repo).map_or(repo, String::as_str)
                    } else {
                        repo
                    };
                    names_fine_grained_repo(repo, node, &self.fine_grained_external_repos, nodes)
                });
            if !fine_grained {
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

pub(crate) fn main_repo_path(label: &str) -> Option<PathBuf> {
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
    /// The owning query's cancellation token; see [`BazelOptions::cancellation`].
    cancellation: Option<Arc<Cancellation>>,
    /// Memoizes resolution per apparent repository name. `resolve` is called once
    /// per external source file and can shell out to Bazel, so without this a
    /// repository with N source files costs N Bazel invocations, all of which
    /// serialize on the Bazel server lock.
    resolved: Arc<Mutex<HashMap<String, PathBuf>>>,
    /// Lazily populated apparent -> canonical repository name mapping.
    repo_mapping: Arc<OnceLock<HashMap<String, String>>>,
    /// Successful root mapping captured during this generation, parsed lazily when needed.
    root_mapping: Option<Vec<u8>>,
    /// Lazily populated canonical repository name -> synthetic `//external:` node name, the
    /// inverse of `repo_mapping`; see [`ExternalRepoResolver::canonical_repo_nodes`].
    canonical_repo_nodes: Arc<OnceLock<HashMap<String, String>>>,
}

impl ExternalRepoResolver {
    fn new(options: &BazelOptions, root_mapping: Option<Vec<u8>>) -> Result<Self> {
        let mut command = options.command_for_hashing();
        command.arg("info").arg("output_base").stdin(Stdio::null());
        let output = cancel::output(options.cancellation.as_deref(), &mut command)
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
            cancellation: options.cancellation.clone(),
            root_mapping,
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
        // Under Bzlmod the directory is named after the canonical repository name
        // (`rules_req_compile++requirements+pip_deps`). `bazel mod dump_repo_mapping`
        // reports that name directly and is one invocation for every repository, so
        // it is consulted before any guess from the apparent name and before the
        // `bazel query` fallback below. The mapping has to come first: an apparent
        // `dep` the root maps to a repository rule (`+_repo_rules+dep`) can coexist
        // with a *module* named `dep`, whose directory `external/dep+` the guess
        // below would pick as soon as anything materialized it (#535).
        let canonical = self.repo_mapping().get(repo);
        if let Some(canonical) = canonical {
            let candidate = external.join(canonical);
            if candidate.exists() {
                return Ok(candidate);
            }
        } else {
            // No mapping entry: a WORKSPACE repository, whose directory carries its
            // own name, or a canonical name given directly (`@@dep+`).
            for candidate in [external.join(repo), external.join(format!("{repo}+"))] {
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
        let mut command = self.command();
        command
            .arg("query")
            .arg(format!("@{repo}//..."))
            .arg("--keep_going")
            .arg("--output")
            .arg("location")
            .stdin(Stdio::null());
        let output = cancel::output(self.cancellation.as_deref(), &mut command)?;
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
        Ok(external.join(canonical.map(String::as_str).unwrap_or(repo)))
    }

    /// Returns the main repository's apparent -> canonical repository mapping,
    /// fetching it at most once. An empty map means the mapping was unavailable
    /// (for example a WORKSPACE-only build), leaving the `bazel query` fallback.
    fn repo_mapping(&self) -> &HashMap<String, String> {
        self.repo_mapping.get_or_init(|| {
            if self
                .cancellation
                .as_deref()
                .is_some_and(Cancellation::is_cancelled)
            {
                return HashMap::new();
            }
            if let Some(root_mapping) = &self.root_mapping {
                return parse_apparent_repo_mapping(&String::from_utf8_lossy(root_mapping));
            }
            let mut command = self.command();
            command
                .arg("mod")
                .arg("dump_repo_mapping")
                .arg("")
                .stdin(Stdio::null());
            match cancel::output(self.cancellation.as_deref(), &mut command) {
                Ok(output) if output.status.success() => {
                    parse_apparent_repo_mapping(&String::from_utf8_lossy(&output.stdout))
                }
                _ => HashMap::new(),
            }
        })
    }

    /// Returns canonical repository name -> the name of the synthetic `//external:<name>` node
    /// `bazel::lower_repositories` emits for it: the root's apparent name for that repository.
    /// A repository mapped under several apparent names has one node per name; the smallest is
    /// chosen so the edge is deterministic. Repositories the root does not map (modules hidden
    /// behind another module, #510) are absent, and their node is named canonically, so callers
    /// fall back to the canonical name itself.
    fn canonical_repo_nodes(&self) -> &HashMap<String, String> {
        self.canonical_repo_nodes.get_or_init(|| {
            let mut nodes = HashMap::<String, String>::new();
            for (apparent, canonical) in self.repo_mapping() {
                match nodes.get(canonical) {
                    Some(existing) if existing <= apparent => {}
                    _ => {
                        nodes.insert(canonical.clone(), apparent.clone());
                    }
                }
            }
            nodes
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
            cancellation: None,
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
    #[cfg(unix)]
    fn generation_reuses_discovery_outputs() {
        let graph = r#"{"key":"<root>","name":"root","dependencies":[]}"#;
        for extension_failure in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let external = workspace.path().join("external/local_config_tool");
            fs::create_dir_all(&external).unwrap();
            fs::write(external.join("input"), "mapped input").unwrap();
            let target = source_target("@tool//:input", &[]);
            let mut encoded = Vec::new();
            target.encode_length_delimited(&mut encoded);
            fs::write(workspace.path().join("targets.pb"), encoded).unwrap();
            let script = workspace.path().join("fake-bazel.sh");
            let graph_result = if extension_failure {
                "echo 'ERROR: Results may be incomplete as 1 extension failed.' >&2; exit 2"
            } else {
                "exit 0"
            };
            fs::write(
                &script,
                format!(
                    r#"#!/bin/sh
case "$1 $2" in
  "mod dump_repo_mapping")
    printf '%s\n' mapping >> "$PWD/mapping-calls"
    printf '%s\n' '{{"tool":"local_config_tool"}}'
    ;;
  "mod graph")
    printf '%s\n' graph >> "$PWD/graph-calls"
    printf '%s\n' '{graph}'
    {graph_result}
    ;;
  "version ") printf '%s\n' 'Build label: 8.7.0' ;;
  "info output_base") printf '%s\n' "$PWD" ;;
  "query "*)
    while [ "$1" != --output_file ]; do shift; done
    cp "$PWD/targets.pb" "$2"
    ;;
  *) exit 80 ;;
esac
"#,
                ),
            )
            .unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            let mut options = hash_options(workspace.path());
            options.bazel.bazel = script;
            options.bazel.no_bazelrc = false;
            options
                .bazel
                .fine_grained_external_repos
                .insert("@tool".into());
            let calls = workspace.path().join("graph-calls");
            let mapping_calls = workspace.path().join("mapping-calls");

            let generated = generate_hashes(&options).unwrap();
            assert_eq!(generated.hashes.len(), 1);
            assert_eq!(generated.module_graph_json.as_deref(), Some(graph));
            assert_eq!(
                fs::read_to_string(&calls).unwrap().lines().count(),
                1,
                "extension_failure={extension_failure}"
            );
            assert_eq!(
                fs::read_to_string(&mapping_calls).unwrap().lines().count(),
                1,
                "the resolver reuses the unfiltered root mapping"
            );

            let direct = hash_targets(&options, vec![target]).unwrap();
            assert_eq!(direct.hashes, generated.hashes);
            assert_eq!(direct.module_graph_json, generated.module_graph_json);
            assert_eq!(
                fs::read_to_string(&calls).unwrap().lines().count(),
                2,
                "direct hashing still reads the graph"
            );
            assert_eq!(
                fs::read_to_string(&mapping_calls).unwrap().lines().count(),
                2,
                "direct hashing still reads the root mapping"
            );
        }
    }

    #[test]
    fn canonical_rule_inputs_are_sorted_and_configuration_sensitive() {
        let rule = Rule {
            rule_input: vec!["//external:repo".into()],
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
            rule_inputs(&rule, true, &BTreeSet::new(), &HashMap::new()),
            [
                String::from("//a:a|1"),
                String::from("//b:b|2"),
                String::from("//external:repo"),
            ]
        );
        assert_eq!(
            rule_inputs(&rule, false, &BTreeSet::new(), &HashMap::new()),
            ["//external:repo"]
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
            transform_external_input("@maven//:guava", &BTreeSet::new(), &HashMap::new()),
            "//external:maven"
        );
    }

    // `bazel cquery` prints canonical labels. The synthetic repository nodes are named by the
    // root's apparent names (or by canonical name for modules the root cannot see, #510), so a
    // canonical input has to be translated or its edge silently vanishes: hashbench (2026-10-18)
    // caught a rules_multitool lockfile bump that query mode reported and cquery mode missed.
    #[test]
    fn canonical_external_inputs_map_to_the_apparent_repository_node() {
        let nodes = HashMap::from([
            ("rules_rs+".to_owned(), "rules_rs".to_owned()),
            (
                "rules_rs++crate+bd_crates".to_owned(),
                "bd_crates".to_owned(),
            ),
            (
                "rules_multitool++multitool+multitool".to_owned(),
                "multitool".to_owned(),
            ),
        ]);
        let none = BTreeSet::new();
        // A module the root declares with bazel_dep.
        assert_eq!(
            transform_external_input("@@rules_rs+//:defs.bzl", &none, &nodes),
            "//external:rules_rs"
        );
        // A repository generated by a module extension, visible under its use_repo name.
        assert_eq!(
            transform_external_input(
                "@@rules_rs++crate+bd_crates//:anyhow-1.0.104",
                &none,
                &nodes
            ),
            "//external:bd_crates"
        );
        assert_eq!(
            transform_external_input(
                "@@rules_multitool++multitool+multitool//tools/foo:foo",
                &none,
                &nodes
            ),
            "//external:multitool"
        );
        // A module the root cannot see keeps its canonical name, matching the node lowered for it.
        assert_eq!(
            transform_external_input("@@rules_cc+//cc:defs.bzl", &none, &nodes),
            "//external:rules_cc+"
        );
        // Main-repository labels are never external, canonical or not.
        assert_eq!(
            transform_external_input("@@//src:bazel-diff", &none, &nodes),
            "@@//src:bazel-diff"
        );
        assert!(is_canonical_external_label("@@rules_rs+//:defs.bzl"));
        assert!(!is_canonical_external_label("@@//src:bazel-diff"));
        assert!(!is_canonical_external_label("@rules_rs//:defs.bzl"));
        assert!(!is_canonical_external_label("//src:bazel-diff"));

        // WORKSPACE-style apparent labels never consult the translation.
        assert_eq!(
            transform_external_input("@rules_rs//:defs.bzl", &none, &nodes),
            "//external:rules_rs"
        );

        // A fine-grained repository is left alone whether the user named it by apparent or by
        // canonical name, in either label form.
        let apparent = BTreeSet::from(["@bd_crates".to_owned()]);
        let canonical = BTreeSet::from(["@@rules_rs++crate+bd_crates".to_owned()]);
        for fine_grained in [&apparent, &canonical] {
            assert_eq!(
                transform_external_input(
                    "@@rules_rs++crate+bd_crates//:anyhow-1.0.104",
                    fine_grained,
                    &nodes
                ),
                "@@rules_rs++crate+bd_crates//:anyhow-1.0.104"
            );
            assert_eq!(
                transform_external_input("@bd_crates//:anyhow-1.0.104", fine_grained, &nodes),
                "@bd_crates//:anyhow-1.0.104"
            );
        }
        // ... but a repository that merely shares a prefix with it is still rewritten.
        assert_eq!(
            transform_external_input("@@rules_rs++crate+bd_crates_other//:x", &apparent, &nodes),
            "//external:rules_rs++crate+bd_crates_other"
        );
    }

    /// Issue #522 end to end through the hasher: `//:consumer` names `@@dep+//:data` (as cquery
    /// prints `@dep//:data`), and `dep` is patched through `single_version_override`, so the
    /// patch is hashed into `//external:dep`. Editing the patch must flip the consumer.
    #[test]
    fn single_version_override_patch_reaches_consumer_under_cquery() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let mut options = hash_options(workspace.path());
        options.bazel.use_cquery = true;
        let hash = |patch_hash: &str| {
            hash_targets_with_environment(
                &options,
                vec![
                    rule_target(Rule {
                        name: "@@//:consumer".into(),
                        rule_class: "filegroup".into(),
                        rule_input: vec!["@@dep+//:data".into()],
                        configured_rule_input: vec![configured_input("@@dep+//:data", Some("aa"))],
                        ..Default::default()
                    }),
                    rule_target(Rule {
                        name: "@@dep+//:data|aa".into(),
                        rule_class: "filegroup".into(),
                        rule_input: vec!["@@dep+//:data.txt".into()],
                        ..Default::default()
                    }),
                    rule_target(Rule {
                        name: "//external:dep".into(),
                        rule_class: "http_archive".into(),
                        attribute: vec![Attribute {
                            name: "_bazel_diff_local_inputs_hash".into(),
                            r#type: attribute::Discriminator::String,
                            string_value: Some(patch_hash.to_owned()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                ],
                HashMap::new(),
                ExternalRepoResolver {
                    workspace: options.bazel.workspace.clone(),
                    bazel: options.bazel.bazel.clone(),
                    output_base: output_base.clone(),
                    repo_mapping: Arc::new(OnceLock::from(HashMap::from([(
                        "dep".to_owned(),
                        "dep+".to_owned(),
                    )]))),
                    ..Default::default()
                },
                Some(r#"{"name":"root"}"#.to_owned()),
            )
            .unwrap()
        };
        let before = hash("patch-v1");
        assert_eq!(
            before.dep_edges["@@//:consumer"],
            ["//external:dep", "@@dep+//:data"]
        );
        assert_eq!(before.dep_edges["@@dep+//:data"], ["//external:dep"]);
        let after = hash("patch-v2");
        assert_ne!(
            before.hashes["@@//:consumer"].hash,
            after.hashes["@@//:consumer"].hash
        );
    }

    /// The fine-grained half of the apparent-name rule: a source file named by canonical label
    /// resolves inside the repository when the user listed the repository's apparent name.
    #[test]
    fn fine_grained_source_resolves_canonical_labels_by_apparent_name() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        let repo_root = output_base
            .join("external")
            .join("rules_jvm_external++maven+maven");
        fs::create_dir_all(&repo_root).unwrap();
        fs::write(repo_root.join("guava.jar"), "jar").unwrap();
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel: PathBuf::from("bazel"),
            output_base,
            repo_mapping: Arc::new(OnceLock::from(HashMap::from([(
                "maven".to_owned(),
                "rules_jvm_external++maven+maven".to_owned(),
            )]))),
            ..Default::default()
        };
        let modified = BTreeSet::new();
        let hasher = |fine_grained: &[&str]| SourceHasher {
            workspace: workspace.path(),
            content_hashes: None,
            modified_filepaths: &modified,
            fine_grained_external_repos: fine_grained.iter().map(|r| (*r).to_owned()).collect(),
            external_resolver: resolver.clone(),
        };
        let label = "@@rules_jvm_external++maven+maven//:guava.jar";
        for fine_grained in [&["maven"][..], &["rules_jvm_external++maven+maven"][..]] {
            let (path, key) = hasher(fine_grained).resolve(label).unwrap().unwrap();
            assert_eq!(path, repo_root.join("guava.jar"), "{fine_grained:?}");
            assert_eq!(
                key, "external/rules_jvm_external++maven+maven/guava.jar",
                "{fine_grained:?}"
            );
        }
        // Neither name listed: the label is opaque, as before.
        assert!(hasher(&["other"]).resolve(label).unwrap().is_none());
        assert!(hasher(&[]).resolve(label).unwrap().is_none());
        // An apparent label, as `query` prints it, resolves under the canonical name too.
        let (path, _) = hasher(&["rules_jvm_external++maven+maven"])
            .resolve("@maven//:guava.jar")
            .unwrap()
            .unwrap();
        assert_eq!(path, repo_root.join("guava.jar"));
        assert!(hasher(&["other"])
            .resolve("@maven//:guava.jar")
            .unwrap()
            .is_none());
    }

    #[test]
    fn canonical_repo_nodes_invert_the_repo_mapping_deterministically() {
        let resolver = ExternalRepoResolver {
            repo_mapping: Arc::new(OnceLock::from(HashMap::from([
                ("rules_rs".to_owned(), "rules_rs+".to_owned()),
                (
                    "bd_crates".to_owned(),
                    "rules_rs++crate+bd_crates".to_owned(),
                ),
                // Two apparent names for one repository: the node picked is the smallest.
                ("zz_alias".to_owned(), "rules_cc+".to_owned()),
                ("cc_rules".to_owned(), "rules_cc+".to_owned()),
            ]))),
            ..Default::default()
        };
        let nodes = resolver.canonical_repo_nodes();
        assert_eq!(nodes["rules_rs+"], "rules_rs");
        assert_eq!(nodes["rules_rs++crate+bd_crates"], "bd_crates");
        assert_eq!(nodes["rules_cc+"], "cc_rules");
        assert_eq!(nodes.len(), 3);
        assert!(std::ptr::eq(nodes, resolver.canonical_repo_nodes()));
    }

    /// End to end through the hasher: under cquery, a main-repository rule whose input lives in
    /// an extension repository reaches that repository's `//external:` node, so a change hashed
    /// into the node (a lock file, a patch) flips the rule.
    #[test]
    fn cquery_rule_reaches_the_repository_node_through_a_canonical_input() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let mut options = hash_options(workspace.path());
        options.bazel.use_cquery = true;
        let targets = |lock_hash: &str| {
            vec![
                rule_target(Rule {
                    name: "@@//src:bazel-diff".into(),
                    rule_class: "rust_binary".into(),
                    rule_input: vec![
                        "@@//src:main.rs".into(),
                        "@@rules_rs++crate+bd_crates//:anyhow-1.0.104".into(),
                    ],
                    configured_rule_input: vec![configured_input(
                        "@@rules_rs++crate+bd_crates//:anyhow-1.0.104",
                        Some("aa"),
                    )],
                    ..Default::default()
                }),
                rule_target(Rule {
                    name: "//external:bd_crates".into(),
                    rule_class: "crate_repository".into(),
                    attribute: vec![Attribute {
                        name: "_bazel_diff_local_inputs_hash".into(),
                        r#type: attribute::Discriminator::String,
                        string_value: Some(lock_hash.to_owned()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            ]
        };
        let hash = |lock_hash: &str| {
            hash_targets_with_environment(
                &options,
                targets(lock_hash),
                HashMap::new(),
                ExternalRepoResolver {
                    workspace: options.bazel.workspace.clone(),
                    bazel: options.bazel.bazel.clone(),
                    output_base: output_base.clone(),
                    repo_mapping: Arc::new(OnceLock::from(HashMap::from([(
                        "bd_crates".to_owned(),
                        "rules_rs++crate+bd_crates".to_owned(),
                    )]))),
                    ..Default::default()
                },
                Some(r#"{"name":"root"}"#.to_owned()),
            )
            .unwrap()
        };
        let before = hash("lock-v1");
        assert_eq!(
            before.dep_edges["@@//src:bazel-diff"],
            ["//external:bd_crates"]
        );
        let after = hash("lock-v2");
        assert_ne!(
            before.hashes["@@//src:bazel-diff"].hash, after.hashes["@@//src:bazel-diff"].hash,
            "the lock file change must reach the consumer in cquery mode"
        );
    }

    // Hub-and-spoke external repos (e.g. rules_python's `@pip` hub with one `@pip_<pkg>` spoke
    // repo per package): a spoke label shares the hub name as a string prefix but is a different
    // repo, so marking the hub fine-grained must not stop spoke inputs from being rewritten to
    // their `//external:<spoke>` synthetic target -- that target is the only node whose hash flips
    // when the spoke's pinned version changes.
    #[test]
    fn fine_grained_repo_name_does_not_prefix_match_spoke_repos() {
        let fine_grained = BTreeSet::from(["@pip".to_owned()]);
        assert_eq!(
            transform_external_input("@pip_numpy//:lib", &fine_grained, &HashMap::new()),
            "//external:pip_numpy"
        );
        assert_eq!(
            transform_external_input("@pip//numpy:pkg", &fine_grained, &HashMap::new()),
            "@pip//numpy:pkg"
        );

        let rule = Rule {
            name: "@pip//numpy:pkg".into(),
            rule_class: "alias".into(),
            rule_input: vec!["@pip_numpy//:lib".into(), "@pip//numpy:other".into()],
            ..Default::default()
        };
        // The spoke input keeps its raw label and gains the seed; the hub-internal input stays raw
        // because the hub itself is fine-grained.
        assert_eq!(
            rule_inputs(&rule, false, &fine_grained, &HashMap::new()),
            [
                "//external:pip_numpy",
                "@pip//numpy:other",
                "@pip_numpy//:lib"
            ]
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
        assert_eq!(tracked.deps.unwrap().as_slice(), ["//dep:a"]);
        assert_ne!(tracked.direct, tracked.overall);

        let mut untracked = TargetDigestBuilder::new(false);
        untracked.direct(b"direct");
        untracked.transitive("//dep:a", b"dependency");
        assert!(untracked.finish().deps.is_none());
        assert_eq!(crate::model::Digest::from([0; 32]).to_string().len(), 64);
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
            transform_external_input("@repo//pkg:file", &fine_grained, &HashMap::new()),
            "@repo//pkg:file"
        );
        assert_eq!(
            transform_external_input("@other//pkg:file", &fine_grained, &HashMap::new()),
            "//external:other"
        );
        assert_eq!(
            transform_external_input("@@other+1.0//pkg:file", &fine_grained, &HashMap::new()),
            "//external:other+1.0"
        );
        assert_eq!(
            transform_external_input("@//pkg:file", &fine_grained, &HashMap::new()),
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
            false,
            None,
            &mut SizeCache::new(),
            &mut included,
        );
        let mut ignored = Sha256::new();
        hash_attribute(
            &attribute,
            &HashSet::from(["cmd".to_owned()]),
            false,
            None,
            &mut SizeCache::new(),
            &mut ignored,
        );
        assert_ne!(included.finalize().to_vec(), ignored.finalize().to_vec());
    }

    #[test]
    fn generated_alias_keeps_the_shared_rules_dependencies() {
        let workspace = tempfile::tempdir().unwrap();
        let rule = |name: &str, inputs: &[&str]| {
            rule_target(Rule {
                name: name.into(),
                rule_class: "genrule".into(),
                rule_input: inputs.iter().map(|input| (*input).to_owned()).collect(),
                ..Default::default()
            })
        };
        // Raw targets can list a rule under a generated file's label. Both rule outputs
        // then read the generating rule's cached digest, including its edge list.
        let data = hash_targets_for_test(
            &hash_options(workspace.path()),
            vec![
                rule("//:a", &["//:dep"]),
                rule("//:dep", &[]),
                rule("//:b", &[]),
                generated_target("//:b", "//:a"),
            ],
            workspace.path(),
        )
        .unwrap();
        assert_eq!(data.dep_edges["//:a"], ["//:dep"]);
        assert_eq!(data.dep_edges["//:b"], ["//:dep", "//:a"]);
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

    #[test]
    fn shared_repository_inputs_match_ordered_traversal_through_a_cycle() {
        let workspace = tempfile::tempdir().unwrap();
        let options = hash_options(workspace.path());
        let group = Arc::new(RepositoryInputGroup {
            extension: "+tools".into(),
            labels: vec!["//external:a".into(), "//external:z".into()],
        });
        let mut targets: Vec<_> = ["a", "hub1", "hub2", "hub3", "z"]
            .into_iter()
            .map(|name| {
                let inputs = match name {
                    "z" => vec!["//external:hub1".into()],
                    "hub1" | "hub2" | "hub3" => group.labels.clone(),
                    _ => Vec::new(),
                };
                rule_target(Rule {
                    name: format!("//external:{name}"),
                    rule_class: "http_archive".into(),
                    rule_input: inputs,
                    ..Default::default()
                })
            })
            .collect();
        let expected = hash_targets_for_test(&options, targets.clone(), workspace.path()).unwrap();
        let mut prepared = HashMap::new();
        for target in &mut targets {
            let rule = target.rule.as_option_mut().unwrap();
            if rule.rule_input == group.labels {
                rule.rule_input.clear();
                prepared.insert(
                    rule.name.clone(),
                    PreparedRule {
                        definition_digest: prehash_rule(rule, &options.ignored_attributes),
                        shared_inputs: Some(Arc::clone(&group)),
                    },
                );
            }
        }
        let actual = hash_targets_with_environment(
            &options,
            targets,
            prepared,
            ExternalRepoResolver::default(),
            None,
        )
        .unwrap();
        assert_eq!(actual.hashes, expected.hashes);
        assert_eq!(actual.dep_edges, expected.dep_edges);
    }

    #[cfg(unix)]
    #[test]
    fn hashing_reports_cycles_and_missing_generators() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let options = hash_options(workspace.path());
        let missing = hash_targets_for_test(
            &options,
            vec![generated_target("//:out", "//:missing")],
            &output_base,
        )
        .unwrap_err()
        .to_string();
        assert!(missing.contains("Unexpected generating rule"));
    }

    #[test]
    fn cycles_hash_deterministically_and_changes_propagate_through_them() {
        fn cyclic_targets(a_cmd: &str, b_cmd: &str) -> Vec<Target> {
            let attribute = |value: &str| Attribute {
                name: "cmd".into(),
                r#type: attribute::Discriminator::String,
                string_value: Some(value.into()),
                ..Default::default()
            };
            vec![
                rule_target(Rule {
                    name: "//:consumer".into(),
                    rule_class: "alias".into(),
                    rule_input: vec!["//:a".into()],
                    ..Default::default()
                }),
                rule_target(Rule {
                    name: "//:a".into(),
                    rule_class: "genrule".into(),
                    rule_input: vec!["//:b".into()],
                    attribute: vec![attribute(a_cmd)],
                    ..Default::default()
                }),
                rule_target(Rule {
                    name: "//:b".into(),
                    rule_class: "genrule".into(),
                    rule_input: vec!["//:a".into()],
                    attribute: vec![attribute(b_cmd)],
                    ..Default::default()
                }),
            ]
        }
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let options = hash_options(workspace.path());

        let baseline =
            hash_targets_for_test(&options, cyclic_targets("a", "b"), &output_base).unwrap();
        // The query order is arbitrary; the hashes must not depend on it.
        let mut reversed = cyclic_targets("a", "b");
        reversed.reverse();
        let shuffled = hash_targets_for_test(&options, reversed, &output_base).unwrap();
        assert_eq!(baseline.hashes, shuffled.hashes);
        assert_eq!(baseline.dep_edges, shuffled.dep_edges);
        // The dependency edges inside the cycle survive for distance metrics and `explain`.
        assert_eq!(baseline.dep_edges["//:a"], ["//:b"]);
        assert_eq!(baseline.dep_edges["//:b"], ["//:a"]);
        assert_eq!(baseline.dep_edges["//:consumer"], ["//:a"]);

        // A change to either member reaches both members and everything downstream.
        for changed in [cyclic_targets("a2", "b"), cyclic_targets("a", "b2")] {
            let hashes = hash_targets_for_test(&options, changed, &output_base)
                .unwrap()
                .hashes;
            for label in ["//:a", "//:b", "//:consumer"] {
                assert_ne!(hashes[label].hash, baseline.hashes[label].hash, "{label}");
            }
        }
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
        fs::create_dir_all(external.join("workspace_repo")).unwrap();
        let log = workspace.path().join("bazel.log");
        // No mapping at all, as in a WORKSPACE build: the directory is guessed from the
        // apparent name, with no `query` fallback for a guess that exists.
        let bazel = stub_bazel(workspace.path(), &log, "");
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel,
            output_base,
            ..Default::default()
        };
        assert_eq!(
            resolver.resolve("existing").unwrap(),
            external.join("existing+")
        );
        assert_eq!(
            resolver.resolve("workspace_repo").unwrap(),
            external.join("workspace_repo")
        );
        let invocations = fs::read_to_string(&log).unwrap();
        assert_eq!(
            invocations.lines().collect::<Vec<_>>(),
            ["mod dump_repo_mapping "]
        );
    }

    /// Tinder/bazel-diff#535: the root maps the apparent `dep` to a repository rule, and a
    /// module also named `dep` (reachable under another apparent name) has been fetched, so
    /// `external/dep+` exists next to `external/+_repo_rules+dep`. The mapping decides, not
    /// the directory a guess from the apparent name happens to find.
    #[cfg(unix)]
    #[test]
    fn external_repo_resolver_prefers_the_repo_mapping_over_a_same_named_module() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        let external = output_base.join("external");
        fs::create_dir_all(external.join("dep+")).unwrap();
        fs::create_dir_all(external.join("+_repo_rules+dep")).unwrap();
        let log = workspace.path().join("bazel.log");
        let bazel = stub_bazel(
            workspace.path(),
            &log,
            r#"{"dep": "+_repo_rules+dep", "module_dep": "dep+"}"#,
        );
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel,
            output_base,
            ..Default::default()
        };
        assert_eq!(
            resolver.resolve("dep").unwrap(),
            external.join("+_repo_rules+dep")
        );
        assert_eq!(
            resolver.resolve("module_dep").unwrap(),
            external.join("dep+")
        );
        // A canonical name given directly is not in the mapping and is found as-is.
        assert_eq!(resolver.resolve("dep+").unwrap(), external.join("dep+"));
        let invocations = fs::read_to_string(&log).unwrap();
        assert_eq!(
            invocations.lines().collect::<Vec<_>>(),
            ["mod dump_repo_mapping "]
        );
    }

    /// The other half of #535: a mapped repository that is not materialized yet is never
    /// substituted by a same-named module's directory. Resolution goes to `bazel query`,
    /// and when that has nothing to say either, the mapped canonical name stands.
    #[cfg(unix)]
    #[test]
    fn external_repo_resolver_never_guesses_for_a_mapped_repository() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        let external = output_base.join("external");
        fs::create_dir_all(external.join("dep+")).unwrap();
        let log = workspace.path().join("bazel.log");
        // Answers `mod dump_repo_mapping` with the mapping and `query` with nothing.
        let bazel = workspace.path().join("stub-bazel");
        fs::write(
            &bazel,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\n\
                 [ \"$1\" = mod ] && echo '{{\"dep\": \"+_repo_rules+dep\"}}'\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bazel, fs::Permissions::from_mode(0o755)).unwrap();
        let resolver = ExternalRepoResolver {
            workspace: workspace.path().to_path_buf(),
            bazel,
            output_base,
            ..Default::default()
        };
        assert_eq!(
            resolver.resolve("dep").unwrap(),
            external.join("+_repo_rules+dep")
        );
        let invocations = fs::read_to_string(&log).unwrap();
        assert_eq!(
            invocations.lines().collect::<Vec<_>>(),
            [
                "mod dump_repo_mapping ",
                "query @dep//... --keep_going --output location",
            ]
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
            // A WORKSPACE-style repository: no mapping, so `external/ext` is found by name
            // without reaching for a Bazel binary.
            repo_mapping: Arc::new(OnceLock::from(HashMap::new())),
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
    fn source_hash_covers_directory_contents() {
        let workspace = tempfile::tempdir().unwrap();
        let directory = workspace.path().join("pkg/testdata");
        fs::create_dir_all(directory.join("nested")).unwrap();
        fs::write(directory.join("a.txt"), "a").unwrap();
        fs::write(directory.join("nested/b.txt"), "b").unwrap();
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
            external_resolver: resolver.clone(),
        };
        let baseline = hasher.digest("//pkg:testdata", b"seed").unwrap();
        assert_eq!(baseline, hasher.digest("//pkg:testdata", b"seed").unwrap());

        // An edit deep inside the tree moves the digest; so does the shape of the tree.
        fs::write(directory.join("nested/b.txt"), "changed").unwrap();
        let edited = hasher.digest("//pkg:testdata", b"seed").unwrap();
        assert_ne!(edited, baseline);
        fs::rename(directory.join("a.txt"), directory.join("c.txt")).unwrap();
        let renamed = hasher.digest("//pkg:testdata", b"seed").unwrap();
        assert_ne!(renamed, edited);
        fs::create_dir(directory.join("empty")).unwrap();
        let with_empty_dir = hasher.digest("//pkg:testdata", b"seed").unwrap();
        assert_ne!(with_empty_dir, renamed);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("c.txt", directory.join("link")).unwrap();
            let with_link = hasher.digest("//pkg:testdata", b"seed").unwrap();
            assert_ne!(with_link, with_empty_dir);
            fs::remove_file(directory.join("link")).unwrap();
            std::os::unix::fs::symlink("nested/b.txt", directory.join("link")).unwrap();
            assert_ne!(hasher.digest("//pkg:testdata", b"seed").unwrap(), with_link);
            fs::remove_file(directory.join("link")).unwrap();
        }

        // A directory is never confused with a missing path or an empty file.
        fs::remove_dir_all(&directory).unwrap();
        let missing = hasher.digest("//pkg:testdata", b"seed").unwrap();
        fs::write(&directory, "").unwrap();
        let empty_file = hasher.digest("//pkg:testdata", b"seed").unwrap();
        fs::remove_file(&directory).unwrap();
        fs::create_dir_all(&directory).unwrap();
        let empty_dir = hasher.digest("//pkg:testdata", b"seed").unwrap();
        assert_ne!(missing, empty_dir);
        assert_ne!(empty_file, empty_dir);

        // With a modified-paths scope, the tree is read only when a listed path is inside it.
        fs::write(directory.join("a.txt"), "a").unwrap();
        let unrelated = BTreeSet::from([PathBuf::from("other/file.txt")]);
        let scoped_out = SourceHasher {
            workspace: workspace.path(),
            content_hashes: None,
            modified_filepaths: &unrelated,
            fine_grained_external_repos: BTreeSet::new(),
            external_resolver: resolver.clone(),
        };
        let skipped = scoped_out.digest("//pkg:testdata", b"seed").unwrap();
        fs::write(directory.join("a.txt"), "edited").unwrap();
        assert_eq!(
            scoped_out.digest("//pkg:testdata", b"seed").unwrap(),
            skipped
        );
        let inside = BTreeSet::from([PathBuf::from("pkg/testdata/a.txt")]);
        let scoped_in = SourceHasher {
            workspace: workspace.path(),
            content_hashes: None,
            modified_filepaths: &inside,
            fine_grained_external_repos: BTreeSet::new(),
            external_resolver: resolver,
        };
        assert_ne!(
            scoped_in.digest("//pkg:testdata", b"seed").unwrap(),
            skipped
        );
        assert_eq!(
            scoped_in.digest("//pkg:testdata", b"seed").unwrap(),
            hasher.digest("//pkg:testdata", b"seed").unwrap()
        );
    }

    #[test]
    fn external_rule_digest_ignores_urls_when_content_is_pinned() {
        fn string(name: &str, value: &str) -> Attribute {
            Attribute {
                name: name.into(),
                r#type: attribute::Discriminator::String,
                string_value: Some(value.into()),
                ..Default::default()
            }
        }
        fn urls(values: &[&str]) -> Attribute {
            Attribute {
                name: "urls".into(),
                r#type: attribute::Discriminator::StringList,
                string_list_value: values.iter().map(|value| (*value).into()).collect(),
                ..Default::default()
            }
        }
        let pinned = Rule {
            name: "//external:dep".into(),
            rule_class: "http_archive".into(),
            attribute: vec![
                urls(&["https://mirror-a/dep.tar.gz"]),
                string("url", "https://mirror-a/dep.tar.gz"),
                string("sha256", "abc"),
            ],
            ..Default::default()
        };
        let mut moved = pinned.clone();
        moved.attribute[0] = urls(&["https://mirror-b/dep.tar.gz"]);
        moved.attribute[1] = string("url", "https://mirror-b/dep.tar.gz");
        let none = HashSet::new();
        assert_eq!(
            rule_definition_digest(&pinned, &none),
            rule_definition_digest(&moved, &none)
        );

        let mut new_content = moved.clone();
        new_content.attribute[2] = string("sha256", "def");
        assert_ne!(
            rule_definition_digest(&moved, &none),
            rule_definition_digest(&new_content, &none)
        );

        // `integrity` pins content the same way; an empty pin does not.
        let mut integrity = pinned.clone();
        integrity.attribute[2] = string("integrity", "sha256-abc");
        let mut integrity_moved = moved.clone();
        integrity_moved.attribute[2] = string("integrity", "sha256-abc");
        assert_eq!(
            rule_definition_digest(&integrity, &none),
            rule_definition_digest(&integrity_moved, &none)
        );
        let mut unpinned = pinned.clone();
        unpinned.attribute[2] = string("sha256", "");
        let mut unpinned_moved = moved.clone();
        unpinned_moved.attribute[2] = string("sha256", "");
        assert_ne!(
            rule_definition_digest(&unpinned, &none),
            rule_definition_digest(&unpinned_moved, &none)
        );

        // Only repository rules are normalized; a main-repo rule keeps its URL attributes.
        let mut main_repo = pinned.clone();
        main_repo.name = "//tools:fetch".into();
        let mut main_repo_moved = moved.clone();
        main_repo_moved.name = "//tools:fetch".into();
        assert_ne!(
            rule_definition_digest(&main_repo, &none),
            rule_definition_digest(&main_repo_moved, &none)
        );
    }

    #[test]
    fn external_rule_digest_ignores_registry_urls_pinned_by_their_integrity() {
        use crate::proto::blaze_query::{StringDictEntry, StringListDictEntry};

        fn dict(name: &str, entries: Vec<(String, String)>) -> Attribute {
            Attribute {
                name: name.into(),
                r#type: attribute::Discriminator::StringDict,
                string_dict_value: entries
                    .into_iter()
                    .map(|(key, value)| StringDictEntry {
                        key,
                        value,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
        }
        // A registry inside the workspace, as Bazel expands `file://%workspace%/registry`.
        fn registry_rule(
            root: &str,
            patches: &[(&str, &str)],
            overlay_integrity: &str,
            module_integrity: &str,
        ) -> Rule {
            Rule {
                name: "//external:dep".into(),
                rule_class: "http_archive".into(),
                attribute: vec![
                    dict(
                        "remote_file_integrity",
                        vec![("BUILD.bazel".into(), overlay_integrity.into())],
                    ),
                    Attribute {
                        name: "remote_file_urls".into(),
                        r#type: attribute::Discriminator::StringListDict,
                        string_list_dict_value: vec![StringListDictEntry {
                            key: "BUILD.bazel".into(),
                            value: vec![format!("file://{root}/registry/overlay/BUILD.bazel")],
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    Attribute {
                        name: "remote_module_file_integrity".into(),
                        r#type: attribute::Discriminator::String,
                        string_value: Some(module_integrity.into()),
                        ..Default::default()
                    },
                    Attribute {
                        name: "remote_module_file_urls".into(),
                        r#type: attribute::Discriminator::StringList,
                        string_list_value: vec![format!("file://{root}/registry/MODULE.bazel")],
                        ..Default::default()
                    },
                    dict(
                        "remote_patches",
                        patches
                            .iter()
                            .map(|(name, integrity)| {
                                (
                                    format!("file://{root}/registry/patches/{name}"),
                                    (*integrity).into(),
                                )
                            })
                            .collect(),
                    ),
                ],
                ..Default::default()
            }
        }
        let none = HashSet::new();
        let patches = [("a.patch", "sha256-a"), ("b.patch", "sha256-b")];
        let at = |root: &str, patches: &[(&str, &str)], overlay: &str, module: &str| {
            rule_definition_digest(&registry_rule(root, patches, overlay, module), &none)
        };
        let here = at(
            "/checkouts/one",
            &patches,
            "sha256-overlay",
            "sha256-module",
        );
        assert_eq!(
            here,
            at(
                "/checkouts/two",
                &patches,
                "sha256-overlay",
                "sha256-module"
            )
        );

        // The pins still count: a changed or reordered patch, or a changed overlay file or
        // MODULE.bazel, is a different repository.
        let changed_patch = [("a.patch", "sha256-a2"), ("b.patch", "sha256-b")];
        let reordered = [("b.patch", "sha256-b"), ("a.patch", "sha256-a")];
        for changed in [
            at(
                "/checkouts/one",
                &changed_patch,
                "sha256-overlay",
                "sha256-module",
            ),
            at(
                "/checkouts/one",
                &reordered,
                "sha256-overlay",
                "sha256-module",
            ),
            at(
                "/checkouts/one",
                &patches,
                "sha256-overlay2",
                "sha256-module",
            ),
            at(
                "/checkouts/one",
                &patches,
                "sha256-overlay",
                "sha256-module2",
            ),
        ] {
            assert_ne!(here, changed);
        }

        // A URL that its integrity does not pin still counts, for each kind on its own.
        let unpinned_patch = [("a.patch", ""), ("b.patch", "sha256-b")];
        for (patches, overlay, module) in [
            (&unpinned_patch[..], "sha256-overlay", "sha256-module"),
            (&patches[..], "", "sha256-module"),
            (&patches[..], "sha256-overlay", ""),
        ] {
            assert_ne!(
                at("/checkouts/one", patches, overlay, module),
                at("/checkouts/two", patches, overlay, module)
            );
        }
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
    fn configured_input(label: &str, checksum: Option<&str>) -> ConfiguredRuleInput {
        ConfiguredRuleInput {
            label: Some(label.to_owned()),
            configuration_checksum: checksum.map(str::to_owned),
            ..Default::default()
        }
    }

    /// Tinder/bazel-diff#512: `//:app` is analyzed twice. In the default configuration it
    /// depends on `//:helper`; `//:helper` transitions it into a bootstrap configuration where
    /// it has no deps. Collapsing the two by label made the edge from `//:helper` point back at
    /// the instance that depends on `//:helper`, which is a cycle only on paper.
    #[test]
    fn configured_instances_of_a_label_are_distinct_nodes() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        fs::write(workspace.path().join("input.txt"), "before").unwrap();
        let mut options = hash_options(workspace.path());
        options.bazel.use_cquery = true;
        let targets = || {
            vec![
                source_target("//:input.txt", &[]),
                rule_target(Rule {
                    name: "//:app|aa".into(),
                    rule_class: "node".into(),
                    configured_rule_input: vec![configured_input("//:helper", Some("aa"))],
                    ..Default::default()
                }),
                rule_target(Rule {
                    name: "//:app|bb".into(),
                    rule_class: "node".into(),
                    ..Default::default()
                }),
                rule_target(Rule {
                    name: "//:helper|aa".into(),
                    rule_class: "helper".into(),
                    configured_rule_input: vec![
                        configured_input("//:app", Some("bb")),
                        configured_input("//:input.txt", None),
                    ],
                    ..Default::default()
                }),
            ]
        };
        let before = hash_targets_for_test(&options, targets(), &output_base).unwrap();
        assert_eq!(
            before.hashes.keys().collect::<Vec<_>>(),
            ["//:app", "//:helper", "//:input.txt"]
        );
        assert_eq!(before.dep_edges["//:app"], ["//:helper"]);
        assert_eq!(before.dep_edges["//:helper"], ["//:app"]);

        fs::write(workspace.path().join("input.txt"), "after").unwrap();
        let after = hash_targets_for_test(&options, targets(), &output_base).unwrap();
        assert_ne!(
            before.hashes["//:helper"].hash,
            after.hashes["//:helper"].hash
        );
        assert_ne!(
            before.hashes["//:app"].hash, after.hashes["//:app"].hash,
            "the change reaches //:app through its default-configuration instance"
        );
    }

    #[test]
    fn a_bare_label_can_collide_with_a_checksum_suffix_in_full_key_order() {
        let rows = [
            ("//:a", "//:first"),
            ("//:a0", "//:middle"),
            ("//:a|ab", "//:last"),
        ]
        .map(|(key, dependency)| {
            (
                key.to_owned(),
                "Rule",
                TargetDigest {
                    overall: [0; 32],
                    direct: [0; 32],
                    deps: Some(Rc::new(vec![dependency.to_owned()])),
                },
            )
        });
        let (hashes, edges) = aggregate_by_label(rows.into(), true);
        assert_eq!(hashes.len(), 2);
        assert_ne!(hashes["//:a"].hash, hashes["//:a0"].hash);
        assert_eq!(edges["//:a"], ["//:first", "//:last"]);
    }

    #[test]
    fn single_configured_instance_hashes_like_a_bare_label() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let mut options = hash_options(workspace.path());
        options.bazel.use_cquery = true;
        let rule = |name: &str| {
            rule_target(Rule {
                name: name.into(),
                rule_class: "node".into(),
                attribute: vec![string_list_attribute("tags", &["t"])],
                ..Default::default()
            })
        };
        let configured =
            hash_targets_for_test(&options, vec![rule("//:a|aa")], &output_base).unwrap();
        let bare = hash_targets_for_test(&options, vec![rule("//:a")], &output_base).unwrap();
        assert_eq!(configured.hashes["//:a"], bare.hashes["//:a"]);
    }

    #[test]
    fn configured_input_outside_the_result_falls_back_to_the_label() {
        let workspace = tempfile::tempdir().unwrap();
        let output_base = workspace.path().join("output");
        fs::create_dir_all(output_base.join("external")).unwrap();
        let mut options = hash_options(workspace.path());
        options.bazel.use_cquery = true;
        // //:lib's edge names an instance of //:tool that is not in the result; it resolves to
        // the instance that is. //:tool's edge back to an absent instance of //:lib would only
        // resolve to the //:lib being hashed, so it is dropped rather than reported as a cycle.
        let data = hash_targets_for_test(
            &options,
            vec![
                rule_target(Rule {
                    name: "//:lib|aa".into(),
                    rule_class: "node".into(),
                    configured_rule_input: vec![configured_input("//:tool", Some("cc"))],
                    ..Default::default()
                }),
                rule_target(Rule {
                    name: "//:tool|bb".into(),
                    rule_class: "node".into(),
                    configured_rule_input: vec![configured_input("//:lib", Some("zz"))],
                    ..Default::default()
                }),
            ],
            &output_base,
        )
        .unwrap();
        assert_eq!(data.dep_edges["//:lib"], ["//:tool"]);
        assert!(data.dep_edges["//:tool"].is_empty());
    }
}
