//! Positive implementation edges for ordinary readers that Bazel resolves at analysis time.
use super::{
    canonical_query_label, query_set, target_label, target_name, BazelOptions, BazelVersion,
    CqueryCapture, Target,
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

type Bindings = BTreeMap<String, BTreeSet<String>>;

#[derive(Default)]
struct ReaderTypes {
    known: Bindings,
    unknown: BTreeSet<String>,
}

/// Bazel emits the class key on each rule and its documentation just once per
/// class. Keep only a local class ID for discovery, outside the hashed rows.
#[derive(Default)]
pub(super) struct PrimaryRules {
    pub(super) alias_discovery_failed: bool,
    classes: HashMap<String, usize>,
    names: BTreeMap<String, Option<usize>>,
}

impl PrimaryRules {
    pub(super) fn record(&mut self, rows: &[Target], encoded: &super::EncodedTargets<'_>) {
        for (index, row) in rows.iter().enumerate() {
            let Some(rule) = row.rule.as_option() else {
                continue;
            };
            let name = super::main_repo_label_key(&rule.name);
            if !name.starts_with("//") || name.starts_with("//external:") {
                continue;
            }
            // The compact rule drops the class key before hashing. Read field
            // 16 from the same source message that provides its attributes.
            let class = encoded
                .get(index)
                .and_then(|bytes| super::single_rule_field(bytes))
                .and_then(|rule| super::last_payload(rule, 16))
                .and_then(|key| std::str::from_utf8(key).ok())
                .or(rule.rule_class_key.as_deref())
                .filter(|key| !key.is_empty())
                .map(|key| {
                    if let Some(&id) = self.classes.get(key) {
                        return id;
                    }
                    let id = self.classes.len();
                    self.classes.insert(key.to_owned(), id);
                    id
                });
            self.names.insert(name.to_owned(), class);
        }
    }
}

struct OrdinaryGraph<'a> {
    mapping: HashMap<String, String>,
    fine: BTreeSet<String>,
    rows: HashMap<String, &'a Target>,
    parents: HashMap<String, Vec<String>>,
    primary: BTreeMap<String, Option<usize>>,
    metadata_classes: HashSet<usize>,
    alias_discovery_failed: bool,
}

impl<'a> OrdinaryGraph<'a> {
    fn new(
        bazel: &BazelOptions,
        targets: &'a [Target],
        primary: PrimaryRules,
        root_mapping: &[u8],
    ) -> Self {
        let mapping =
            crate::hash::parse_apparent_repo_mapping(&String::from_utf8_lossy(root_mapping));
        let fine = bazel
            .fine_grained_external_repos
            .iter()
            .filter_map(|name| {
                name.strip_prefix("@@")
                    .or_else(|| {
                        mapping
                            .get(name.trim_start_matches('@'))
                            .map(String::as_str)
                    })
                    .map(str::to_owned)
            })
            .collect();
        let mut graph = Self {
            mapping,
            fine,
            rows: HashMap::new(),
            parents: HashMap::new(),
            primary: primary.names,
            metadata_classes: ["toolchain", "toolchain_type", "alias"]
                .into_iter()
                .filter_map(|key| primary.classes.get(key).copied())
                .collect(),
            alias_discovery_failed: primary.alias_discovery_failed,
        };
        graph.index(targets);
        graph
    }

    fn index(&mut self, targets: &'a [Target]) {
        for target in targets {
            let Some(name) = target_name(target) else {
                continue;
            };
            let id = self.canonical(name);
            if id.starts_with("//external:") {
                continue;
            }
            self.rows.insert(id.clone(), target);
            for child in super::target_inputs(target) {
                self.parents
                    .entry(self.canonical(child))
                    .or_default()
                    .push(id.clone());
            }
        }
    }

    fn canonical(&self, name: &str) -> String {
        canonical_query_label(name, &self.mapping).into_owned()
    }

    fn ancestors(
        &self,
        mut names: BTreeSet<String>,
        admit: impl Fn(&str) -> bool,
    ) -> BTreeSet<String> {
        let mut pending: Vec<_> = names.iter().cloned().collect();
        while let Some(name) = pending.pop() {
            for parent in self.parents.get(&name).into_iter().flatten() {
                if admit(parent) && names.insert(parent.clone()) {
                    pending.push(parent.clone());
                }
            }
        }
        names
    }
}

fn repository(label: &str) -> Option<&str> {
    label
        .strip_prefix("@@")?
        .split_once("//")
        .map(|(repo, _)| repo)
}

/// Select matching labels from the configured universe without analyzing them as
/// independent top-level targets. A literal target pattern can otherwise select
/// its top-level configuration instead of the one reached from the caller.
fn in_universe(names: &BTreeSet<String>) -> String {
    let alternatives = names
        .iter()
        .map(|name| regex_literal(name.trim_start_matches('@')))
        .collect::<Vec<_>>();
    format!("filter('^@{{0,2}}(?:{})$', $g)", alternatives.join("|"))
}

fn regex_literal(name: &str) -> String {
    use std::fmt::Write;
    let mut literal = String::new();
    for ch in name.chars() {
        if ch.is_alphanumeric() || "_/-:".contains(ch) {
            literal.push(ch);
        } else {
            write!(&mut literal, "\\x{{{:x}}}", u32::from(ch)).expect("write to String");
        }
    }
    literal
}

// A projected DOT result can contract paths. The unprojected one-hop graph
// emits a separate node record even when no edges leave a target.
fn direct_type_edges(
    text: &str,
    mapping: &HashMap<String, String>,
    labels: &BTreeSet<String>,
    types: &BTreeSet<String>,
) -> Option<Bindings> {
    let body = text
        .trim()
        .strip_prefix("digraph mygraph {")?
        .strip_suffix('}')?;
    let mut result = Bindings::new();
    let mut edges = Bindings::new();
    for line in body.lines().map(str::trim) {
        if line.is_empty() || line == "node [shape=box];" {
            continue;
        }
        let value = line.strip_prefix('"')?.strip_suffix('"')?;
        let (parent, child) = value
            .split_once("\" -> \"")
            .map_or((value, None), |(parent, child)| (parent, Some(child)));
        // Bazel's DOT writer prints label text without escaping it. Never use
        // an ambiguous or incomplete record to rule out a dependency.
        if [Some(parent), child]
            .into_iter()
            .flatten()
            .any(|label| label.contains(['"', '\\']))
        {
            return None;
        }
        let name = canonical_query_label(parent, mapping);
        if labels.contains(name.as_ref()) {
            if let Some(child) = child {
                let child = canonical_query_label(child, mapping);
                if types.contains(child.as_ref()) {
                    edges
                        .entry(name.into_owned())
                        .or_default()
                        .insert(child.into_owned());
                }
            } else {
                result.entry(name.into_owned()).or_default();
            }
        }
    }
    for (name, observed) in &mut result {
        *observed = edges.remove(name).unwrap_or_default();
    }
    Some(result)
}

/// Every path lost by removing a membership has a first removed edge. Compute its
/// possible predecessors on the repository nodes actually returned by the lowerer.
fn affected_repositories(
    rows: &[super::RepositoryRow],
    usages: &BTreeSet<String>,
) -> HashSet<String> {
    let mut parents = HashMap::<&str, Vec<&str>>::new();
    let mut groups = HashSet::new();
    let mut affected = HashSet::new();
    for row in rows {
        let rule = row.target.rule.as_option().expect("repository");
        let name = rule.name.as_str();
        for child in &rule.rule_input {
            parents.entry(child).or_default().push(name);
        }
        for group in &row.memberships {
            if usages.contains(&group.extension) {
                if row.inferred(group).next().is_some() {
                    affected.insert(name);
                }
            } else if row.generated {
                let key = group.extension.as_str();
                parents.entry(key).or_default().push(name);
                if groups.insert(key) {
                    for child in &group.labels {
                        parents.entry(child).or_default().push(key);
                    }
                }
            } else {
                for child in &group.labels {
                    parents.entry(child).or_default().push(name);
                }
            }
        }
    }
    let mut pending: Vec<_> = affected.iter().copied().collect();
    while let Some(id) = pending.pop() {
        for &parent in parents.get(id).into_iter().flatten() {
            if affected.insert(parent) {
                pending.push(parent);
            }
        }
    }
    affected.into_iter().map(str::to_owned).collect()
}

/// Keep the old membership only on callers whose ordinary input or configured
/// analysis was unavailable. Ordinary module dependencies stay on their original nodes.
fn retain_memberships(
    bindings: &mut Bindings,
    repositories: &[super::RepositoryRow],
    family: &str,
    callers: BTreeSet<String>,
) {
    if callers.is_empty() {
        return;
    }
    let members: BTreeSet<_> = repositories
        .iter()
        .flat_map(|row| {
            row.memberships
                .iter()
                .filter(|group| group.extension == family)
                .flat_map(|group| row.inferred(group))
        })
        .collect();
    for caller in callers {
        bindings
            .entry(caller)
            .or_default()
            .extend(members.iter().map(|label| (*label).to_owned()));
    }
}

impl BazelOptions {
    pub(super) fn observe_external_inputs(
        &self,
        targets: &[Target],
        primary: PrimaryRules,
        mapping: &[u8],
        survey: Option<&super::ExtensionRepositories>,
        repository_rows: &[super::RepositoryRow],
        version: Option<BazelVersion>,
    ) -> Result<ExtensionInputs> {
        let graph = OrdinaryGraph::new(self, targets, primary, mapping);
        let version = match version {
            Some(version) => version,
            None => self.version()?,
        };
        let mut options = self.clone();
        options.keep_going = false;
        options.command_options.extend(
            [
                "--nokeep_going",
                "--implicit_deps",
                "--tool_deps",
                "--proto:rule_inputs_and_outputs",
            ]
            .map(str::to_owned),
        );
        options.cquery_options.extend(
            [
                "--nokeep_going",
                "--skip_incompatible_explicit_targets",
                "--implicit_deps",
                "--tool_deps",
                "--proto:include_configurations",
                "--proto:rule_inputs_and_outputs",
            ]
            .map(str::to_owned),
        );
        let bindings = options.resolved_toolchain_inputs(&graph, targets, mapping, version)?;
        let mut result = ExtensionInputs::default();
        if let Some(repositories) = survey {
            let mut recovered = Vec::new();
            match options.extension_inputs(
                graph,
                repositories,
                repository_rows,
                version,
                &mut recovered,
            ) {
                Ok(observed) => result = observed,
                Err(error) => {
                    self.check_cancelled()?;
                    eprintln!("[Warn] retaining inferred external dependencies: {error:#}");
                }
            }
            // Recovery and removal of the extension's inferred edges are one update.
            // Publishing the new rows against the old memberships adds sibling dependencies.
            if !result.usages.is_empty() {
                super::retain_supplemental_targets(&mut result.sources, &recovered);
                result.sources.extend(recovered);
            }
        }
        for (reader, inputs) in bindings {
            result.bindings.entry(reader).or_default().extend(inputs);
        }
        Ok(result)
    }

    fn configured_children(
        &self,
        graph: &OrdinaryGraph<'_>,
        query: &str,
        version: BazelVersion,
        capture: CqueryCapture<'_>,
    ) -> Result<Bindings> {
        let mut inputs = Bindings::new();
        let observed = self.query_with_capture(
            query,
            true,
            Some(version),
            false,
            &mut |_, _| {},
            capture,
            |_| true,
        )?;
        for row in observed.targets {
            if let Some(rule) = row.rule.as_option() {
                let reader = graph.canonical(target_label(&rule.name));
                inputs.entry(reader).or_default().extend(
                    rule.configured_rule_input
                        .iter()
                        .filter_map(|input| input.label.as_deref())
                        .map(|label| graph.canonical(label)),
                );
            } else if let Some(output) = row.generated_file.as_option() {
                inputs
                    .entry(graph.canonical(target_label(&output.name)))
                    .or_default()
                    .insert(graph.canonical(target_label(&output.generating_rule)));
            } else if let Some(name) = target_name(&row) {
                inputs
                    .entry(graph.canonical(target_label(name)))
                    .or_default();
            }
        }
        Ok(inputs)
    }

    fn readers_of(
        &self,
        graph: &OrdinaryGraph<'_>,
        types: &BTreeSet<String>,
    ) -> Result<ReaderTypes> {
        let mut readers = ReaderTypes::default();
        // Ordinary proto reports per-target label attributes, but omits the
        // toolchain types declared on the rule class. Bazel's loading graph
        // visits both, but does not enumerate requirements exclusive to named
        // execution groups. An absent class key gets its own observation.
        let mut classes = BTreeMap::<_, Vec<&str>>::new();
        for (name, class) in &graph.primary {
            // These native rules describe or forward a toolchain. Their normal
            // inputs already include the declaration or aliased target.
            if class.is_some_and(|id| graph.metadata_classes.contains(&id)) {
                continue;
            }
            let Some(rule) = graph.rows.get(name).and_then(|row| row.rule.as_option()) else {
                continue;
            };
            let explicit: BTreeSet<_> = rule
                .rule_input
                .iter()
                .filter_map(|input| {
                    types
                        .get(canonical_query_label(input, &graph.mapping).as_ref())
                        .cloned()
                })
                .collect();
            // A direct edge can come from the class or this instance. Only
            // share the observed union with instances having the same inputs
            // among these types.
            classes
                .entry((class, class.is_none().then_some(name), explicit))
                .or_default()
                .push(name);
        }
        let mut groups = BTreeMap::new();
        for members in classes.into_values() {
            let first = members[0];
            groups.insert(first.to_owned(), members);
        }
        // rdeps(roots, types, 1) preloads the transitive closure of roots.
        // Query only immediate inputs. Isolate an unavailable representative
        // so the other classes still get a complete observation.
        let representatives: Vec<_> = groups.keys().cloned().collect();
        let mut pending = vec![representatives.as_slice()];
        while let Some(batch) = pending.pop().filter(|batch| !batch.is_empty()) {
            let observations = self.direct_types(graph, batch, types)?;
            if observations.is_none() && batch.len() > 1 {
                let (left, right) = batch.split_at(batch.len() / 2);
                pending.extend([right, left]);
                continue;
            }
            for representative in batch {
                let members = &groups[representative];
                let mut types_seen = observations
                    .as_ref()
                    .and_then(|map| map.get(representative))
                    .cloned();
                // An unrelated missing attribute on one instance does not change
                // requirements declared by its rule class. Another instance can
                // witness those requirements even when the first cannot load.
                if types_seen.is_none() && observations.as_ref().is_none_or(|map| !map.is_empty()) {
                    for member in members.iter().skip(1) {
                        let name = (*member).to_owned();
                        types_seen = self
                            .direct_types(graph, std::slice::from_ref(&name), types)?
                            .and_then(|mut edges| edges.remove(&name));
                        if types_seen.is_some() {
                            break;
                        }
                    }
                }
                match types_seen {
                    Some(observed) if !observed.is_empty() => {
                        for &member in members {
                            readers.known.insert(member.to_owned(), observed.clone());
                        }
                    }
                    Some(_) => {}
                    None => {
                        eprintln!("[Warn] retaining possible toolchain implementations for {} rules represented by {representative}", members.len());
                        readers
                            .unknown
                            .extend(members.iter().map(|name| (*name).to_owned()));
                    }
                }
            }
        }
        if self.verbose {
            eprintln!("[Info] Toolchain types examined {} main rules in {} class observations: {} nominated, {} unresolved", graph.primary.len(), representatives.len(), readers.known.len(), readers.unknown.len());
        }
        Ok(readers)
    }

    fn direct_types(
        &self,
        graph: &OrdinaryGraph<'_>,
        labels: &[String],
        types: &BTreeSet<String>,
    ) -> Result<Option<Bindings>> {
        let labels = labels.iter().cloned().collect::<BTreeSet<_>>();
        let Ok(query) = query_set(&labels) else {
            return Ok(None);
        };
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), format!("deps({query}, 1)"))?;
        let mut command = self.command();
        command
            .arg("query")
            .args(&self.command_options)
            .args([
                "--output=graph",
                "--nograph:factored",
                "--graph:conditional_edges_limit=0",
                "--order_output=deps",
                "--consistent_labels",
                "--query_file",
            ])
            .arg(file.path());
        let output = self.run(&mut command)?;
        if !output.status.success() {
            self.check_cancelled()?;
            if matches!(output.status.code(), Some(3 | 7)) {
                return Ok(None);
            }
            anyhow::bail!(
                "could not inspect direct toolchain inputs: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let text = std::str::from_utf8(&output.stdout).ok();
        Ok(Some(
            text.and_then(|text| direct_type_edges(text, &graph.mapping, &labels, types))
                .unwrap_or_default(),
        ))
    }

    fn resolved_toolchain_inputs(
        &self,
        graph: &OrdinaryGraph<'_>,
        targets: &[Target],
        root_mapping: &[u8],
        version: BazelVersion,
    ) -> Result<Bindings> {
        let fine = graph
            .rows
            .keys()
            .chain(graph.parents.keys())
            .filter(|name| repository(name).is_some_and(|repo| graph.fine.contains(repo)))
            .cloned()
            .collect();
        let reached = graph.ancestors(fine, |_| true);
        if graph.primary.is_empty() || reached.is_empty() {
            return Ok(Bindings::new());
        }
        let observations = self.observed_toolchains(targets, Some(root_mapping), false)?;
        let mut implementations = BTreeMap::new();
        let mut by_type = Bindings::new();
        for declaration in observations.declarations.values() {
            for name in declaration
                .implementations
                .iter()
                .filter(|name| reached.contains(*name))
            {
                if let Some(actual) = graph.rows.get(name).and_then(|t| target_name(t)) {
                    implementations.insert(name.clone(), actual.to_owned());
                    for ty in &declaration.types {
                        by_type
                            .entry(ty.clone())
                            .or_default()
                            .insert(actual.to_owned());
                    }
                }
            }
        }
        let mut pending: BTreeSet<_> = by_type.keys().cloned().collect();
        if pending.is_empty() {
            return Ok(Bindings::new());
        }
        while !pending.is_empty() {
            let aliases = self.alias_actuals(&pending, version)?;
            pending.clear();
            for (alias, actual) in aliases {
                let inputs = by_type[&graph.canonical(&alias)].clone();
                for label in super::scalar_label_values(&actual) {
                    let target = graph.canonical(label);
                    let candidates = by_type.entry(target.clone()).or_default();
                    let before = candidates.len();
                    candidates.extend(inputs.iter().cloned());
                    if candidates.len() != before {
                        pending.insert(target);
                    }
                }
            }
        }
        let types = by_type.keys().cloned().collect();
        let readers = self.readers_of(graph, &types)?;
        let mut bindings = Bindings::new();
        for name in readers.unknown {
            let spelling = target_name(graph.rows[&name]).expect("primary rule");
            bindings.insert(
                spelling.to_owned(),
                implementations.values().cloned().collect(),
            );
        }
        // The ordinary graph combines configurations. An observed type can
        // resolve to different suppliers under different incoming transitions.
        // Keep possible suppliers without analyzing every main-repository reader.
        for (id, types) in readers.known {
            let name = target_name(graph.rows[&id]).expect("primary rule");
            bindings
                .entry(name.to_owned())
                .or_default()
                .extend(types.iter().flat_map(|ty| by_type[ty].iter().cloned()));
        }
        Ok(bindings)
    }
}

#[derive(Default)]
pub(super) struct ExtensionInputs {
    pub(super) usages: BTreeSet<String>,
    pub(super) bindings: BTreeMap<String, BTreeSet<String>>,
    pub(super) sources: Vec<Target>,
}

impl BazelOptions {
    /// Observe the fine repositories where actual ordinary edges enter them.
    /// Analysis supplies the selected direct prerequisites of the reached rules;
    /// a failed lookup leaves the repository lowering on its existing inputs.
    fn extension_inputs(
        &self,
        graph: OrdinaryGraph<'_>,
        repositories: &super::ExtensionRepositories,
        repository_rows: &[super::RepositoryRow],
        version: BazelVersion,
        recovered: &mut Vec<Target>,
    ) -> Result<ExtensionInputs> {
        let usages: BTreeSet<_> = graph
            .fine
            .intersection(&repositories.generated)
            .filter_map(|repo| super::extension_usage_prefix(repo))
            .map(str::to_owned)
            .collect();
        let usage = |name: &str| {
            let family = super::extension_usage_prefix(repository(name)?)?;
            usages.get(family).cloned()
        };
        let mut result = ExtensionInputs::default();
        if usages.is_empty() {
            return Ok(result);
        }
        let direct: BTreeSet<_> = graph
            .parents
            .iter()
            .filter(|(child, callers)| {
                usage(child).is_some()
                    && callers
                        .iter()
                        .any(|parent| graph.primary.contains_key(parent))
            })
            .map(|(child, _)| child.clone())
            .collect();
        let mut seen = BTreeSet::new();
        let mut readers = BTreeSet::new();
        let mut visit = |graph: &OrdinaryGraph<'_>, mut pending: Vec<String>| {
            let mut missing = BTreeSet::new();
            while let Some(name) = pending.pop() {
                let Some(family) = usage(&name) else { continue };
                let Some(row) = graph.rows.get(&name) else {
                    missing.insert(name);
                    continue;
                };
                if !seen.insert(name.clone()) {
                    continue;
                }
                result.usages.insert(family);
                if let Some(rule) = row.rule.as_option() {
                    if rule.rule_class != "alias" {
                        readers.insert(name);
                    }
                    pending.extend(rule.rule_input.iter().map(|child| graph.canonical(child)));
                } else if let Some(file) = row.generated_file.as_option() {
                    pending.push(graph.canonical(&file.generating_rule));
                }
            }
            missing
        };
        let mut missing = visit(&graph, direct.iter().cloned().collect());
        let mut graph = graph;
        if !missing.is_empty() {
            // A failed keep-going alias lookup deliberately commits none of
            // its forwarding rows. Preserve that decision for those operands.
            let recoverable = missing
                .iter()
                .filter(|name| {
                    !graph.alias_discovery_failed
                        || !graph
                            .parents
                            .get(*name)
                            .into_iter()
                            .flatten()
                            .any(|parent| {
                                graph
                                    .rows
                                    .get(parent)
                                    .and_then(|row| row.rule.as_option())
                                    .is_some_and(|rule| rule.rule_class == "alias")
                            })
                })
                .cloned()
                .collect();
            *recovered = self.recover_ordinary_inputs(&recoverable, version)?;
            recovered.retain(|row| {
                target_name(row)
                    .is_some_and(|name| !graph.rows.contains_key(&graph.canonical(name)))
            });
            graph.index(recovered);
            missing = visit(&graph, missing.into_iter().collect());
        }
        // A target that names a removed alias can be present in an ordinary
        // wildcard query even though analyzing it would fail. Its readers
        // retain the membership candidates that covered the unavailable input.
        let unavailable = graph.ancestors(missing.clone(), |_| true);
        if !missing.is_empty() {
            let mut callers = BTreeMap::<String, BTreeSet<String>>::new();
            for name in &missing {
                let family = usage(name).expect("missing extension input");
                let readers = callers.entry(family).or_default();
                for parent in graph.parents.get(name).into_iter().flatten() {
                    anyhow::ensure!(
                        graph.rows.get(parent).is_some_and(|row| row.rule.is_set()),
                        "cannot retain repository inputs for {parent} reading {name}"
                    );
                    readers.insert(parent.clone());
                }
            }
            for (family, readers) in callers {
                retain_memberships(&mut result.bindings, repository_rows, &family, readers);
            }
            if self.verbose {
                eprintln!(
                    "[Info] retaining extension membership on readers of {} unavailable inputs",
                    missing.len()
                );
            }
        }
        let canonical = |label: &str| canonical_query_label(label, &graph.mapping).into_owned();
        if result.usages.is_empty() {
            return Ok(result);
        }
        // The direct consumer may itself be reached through a transition. Analyze the
        // callers that lead to it as well, asking Bazel to emit only the reached rows.
        let nodes = crate::hash::canonical_repo_nodes(&graph.mapping);
        let ingress = |families: &BTreeSet<String>| {
            let in_family = |label: &str| usage(label).is_some_and(|f| families.contains(&f));
            let affected = affected_repositories(repository_rows, families);
            let companion = |label: &str| {
                let target = crate::hash::transform_external_input(
                    label,
                    &self.fine_grained_external_repos,
                    &nodes,
                );
                affected.contains(target.as_ref())
            };
            let mut entries = BTreeSet::new();
            let mut boundary = readers
                .iter()
                .filter(|name| in_family(name))
                .cloned()
                .collect::<BTreeSet<_>>();
            let mut loaded_packages = HashSet::new();
            for (name, row) in &graph.rows {
                if let Some(source) = row.source_file.as_option() {
                    if source.subinclude.iter().any(|load| companion(load)) {
                        loaded_packages.insert(crate::hash::label_package(name));
                    }
                }
                for input in super::target_inputs(row).filter(|input| companion(input)) {
                    boundary.insert(name.clone());
                    let child = canonical(input);
                    if repository(&child).is_some()
                        && !graph
                            .rows
                            .get(&child)
                            .is_some_and(|row| row.source_file.is_set())
                    {
                        entries.insert(child);
                    }
                }
            }
            boundary.extend(
                graph
                    .rows
                    .keys()
                    .filter(|name| loaded_packages.contains(crate::hash::label_package(name)))
                    .cloned(),
            );
            let possible = graph.ancestors(boundary, |_| true);
            let direct_callers = direct
                .iter()
                .filter(|child| in_family(child))
                .flat_map(|child| &graph.parents[child]);
            let roots = possible
                .iter()
                .chain(direct_callers)
                .filter(|label| graph.primary.contains_key(*label))
                .cloned()
                .collect();
            let mut roots = graph.ancestors(roots, |name| graph.primary.contains_key(name));
            roots.retain(|name| !unavailable.contains(name));
            (possible, entries, roots)
        };
        let (possible, entries, mut roots) = ingress(&result.usages);
        if roots.is_empty() {
            return Ok(ExtensionInputs::default());
        }
        // Strict analysis certifies the paths used to remove a membership. If independent
        // callers fail, identify available roots with keep-going and certify that subset.
        let discover = |roots: &BTreeSet<String>| -> Result<_> {
            let callers = query_set(roots)?;
            let scope = format!("deps({callers})");
            let output = self.query_formatted(&scope,
                "def format(target):\n    return str(target.label) if 'ToolchainInfo' in (providers(target) or {}) else ''\n", version)?;
            Ok((callers, scope, output))
        };
        let (mut callers, mut scope, mut output) = discover(&roots)?;
        let mut omitted = BTreeSet::new();
        if matches!(output.status.code(), Some(1 | 3 | 7)) {
            self.check_cancelled()?;
            let mut options = self.clone();
            options.cquery_options.push("--keep_going".to_owned());
            let available = options.query_compatible_labels(&callers, version, true)?;
            omitted.extend(
                roots
                    .iter()
                    .filter(|root| !available.contains(super::main_repo_label_key(root)))
                    .cloned(),
            );
            if !omitted.is_empty() && omitted.len() < roots.len() {
                roots.retain(|root| !omitted.contains(root));
                (callers, scope, output) = discover(&roots)?;
            }
        }
        anyhow::ensure!(
            output.status.success(),
            "could not examine configured toolchains: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let providers = std::str::from_utf8(&output.stdout)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(canonical)
            .collect::<BTreeSet<_>>();
        let candidates: BTreeSet<_> = possible
            .iter()
            .filter(|name| graph.rows.get(*name).is_some_and(|row| row.rule.is_set()))
            .cloned()
            .collect();
        let opaque = if entries.is_empty() {
            String::new()
        } else {
            // A rule behind an ordinary repository input can return a generated file directly.
            // Keep the path to the first real dependency in the observed extension, as well as
            // paths to toolchains whose implementations belong to a different extension.
            let families = result
                .usages
                .iter()
                .map(|name| regex_literal(name))
                .collect::<Vec<_>>()
                .join("|");
            let members = format!("filter('^@{{1,2}}(?:{families})[+][^/]+//', $g)");
            format!(" + (let members = {members} in (allpaths({}, ($p + (rdeps($g, $members, 1) except $members))) except $p))", in_universe(&entries))
        };
        let expression = format!("let g = {scope} in let p = {} in ((rdeps($g, $p, 1) intersect {}){opaque} + {callers})", in_universe(&providers), in_universe(&candidates));
        let configured =
            self.configured_children(&graph, &expression, version, CqueryCapture::Standard)?;
        // An incompatible explicit target may also be omitted by a successful cquery.
        if !roots.iter().any(|root| configured.contains_key(root)) {
            return Ok(ExtensionInputs::default());
        }
        omitted.extend(
            roots
                .iter()
                .filter(|root| !configured.contains_key(*root))
                .cloned(),
        );
        if !omitted.is_empty() {
            for family in &result.usages {
                let callers = if result.usages.len() == 1 {
                    omitted.clone()
                } else {
                    ingress(&BTreeSet::from([family.clone()]))
                        .2
                        .intersection(&omitted)
                        .cloned()
                        .collect()
                };
                retain_memberships(&mut result.bindings, repository_rows, family, callers);
            }
            if self.verbose {
                eprintln!(
                    "[Info] retaining inferred dependencies on {} unavailable configured callers",
                    omitted.len()
                );
            }
        }
        let mut missing = BTreeSet::new();
        for (name, children) in &configured {
            let mut inputs = BTreeSet::new();
            for child in children {
                if !providers.contains(child)
                    && !configured.contains_key(child)
                    && usage(child).is_none()
                {
                    continue;
                }
                if !graph.rows.contains_key(child) {
                    missing.insert(child.clone());
                }
                inputs.insert(child.clone());
            }
            if !inputs.is_empty() {
                if !graph.rows.contains_key(name) {
                    missing.insert(name.clone());
                }
                result
                    .bindings
                    .entry(name.clone())
                    .or_default()
                    .extend(inputs);
            }
        }
        let mut needed = BTreeSet::new();
        if !missing.is_empty() {
            // A selected toolchain can have dependencies on other platforms in its
            // ordinary rule. Ask for the configurations reached by these callers.
            let selected = self.configured_children(
                &graph,
                &format!("let g = {scope} in deps({})", in_universe(&missing)),
                version,
                CqueryCapture::Standard,
            )?;
            for name in &missing {
                anyhow::ensure!(
                    selected.contains_key(name),
                    "missing configured input {name}"
                );
            }
            for child in selected.values().flatten() {
                anyhow::ensure!(
                    selected.contains_key(child) || graph.rows.contains_key(child),
                    "missing configured prerequisite {child}"
                );
            }
            needed = selected
                .keys()
                .filter(|name| !graph.rows.contains_key(*name))
                .cloned()
                .collect();
            let expression = format!("let d = {} in $d + buildfiles($d)", query_set(&needed)?);
            result.sources =
                self.query_with(&expression, false, Some(version), false, &mut |_, _| {})?;
            // Only these new rows were recovered from configured occurrences.
            // Existing ordinary targets keep their complete inputs. Buildfiles in
            // this response still supply the package's evaluated load inputs.
            for row in &mut result.sources {
                if let Some(rule) = row.rule.as_option_mut() {
                    if let Some(children) = selected.get(&canonical(&rule.name)) {
                        rule.rule_input
                            .retain(|child| children.contains(&canonical(child)));
                    }
                }
            }
            for (name, children) in selected {
                if !children.is_empty() {
                    result.bindings.entry(name).or_default().extend(children);
                }
            }
        }
        // Configured labels are canonical. Install each edge using the names of
        // the ordinary targets actually retained (including newly recovered rows).
        let mut rows = graph.rows;
        rows.extend(
            result
                .sources
                .iter()
                .filter_map(|row| Some((canonical(target_name(row)?), row))),
        );
        for name in needed {
            anyhow::ensure!(
                rows.contains_key(&name),
                "missing ordinary target for configured input {name}"
            );
        }
        let spelling = |name: String| {
            rows.get(&name)
                .and_then(|row| target_name(row))
                .map(str::to_owned)
                .unwrap_or(name)
        };
        let mut bindings = Bindings::new();
        for (name, children) in result.bindings {
            bindings
                .entry(spelling(name))
                .or_default()
                .extend(children.into_iter().map(&spelling));
        }
        result.bindings = bindings;
        Ok(result)
    }

    fn recover_ordinary_inputs(
        &self,
        names: &BTreeSet<String>,
        version: BazelVersion,
    ) -> Result<Vec<Target>> {
        let mut result = Vec::new();
        let names: Vec<_> = names.iter().cloned().collect();
        let mut pending = vec![names.as_slice()];
        while let Some(batch) = pending.pop().filter(|batch| !batch.is_empty()) {
            let query = query_set(&batch.iter().cloned().collect())?;
            let expression = format!("let d = deps({query}) in $d + buildfiles($d)");
            match self.query_with(&expression, false, Some(version), false, &mut |_, _| {}) {
                Ok(rows) => result.extend(rows),
                Err(error) => {
                    self.check_cancelled()?;
                    if !error
                        .downcast_ref::<super::QueryFailure>()
                        .is_some_and(|failure| matches!(failure.code, 3 | 7))
                    {
                        return Err(error);
                    }
                    if batch.len() > 1 {
                        let (left, right) = batch.split_at(batch.len() / 2);
                        pending.extend([right, left]);
                    } else {
                        eprintln!(
                            "[Warn] retaining repository inputs for {}: {error:#}",
                            batch[0]
                        );
                    }
                }
            }
        }
        super::deduplicate_targets(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_type_edges_require_a_complete_unambiguous_node_record() {
        let mapping = HashMap::new();
        let labels = BTreeSet::from(["//:reader".into(), "//:wrapper".into()]);
        let types = BTreeSet::from(["//:type".into()]);
        let query = |body: &str| direct_type_edges(body, &mapping, &labels, &types);
        let valid = r#"digraph mygraph {
  node [shape=box];
  "@@//:reader"
  "@@//:reader" -> "@@//:type"
  "@@//:wrapper"
  "@@//:wrapper" -> "@@//:reader"
}"#;
        let edges = query(valid).unwrap();
        assert_eq!(edges["//:reader"], types);
        assert!(edges["//:wrapper"].is_empty());
        assert!(query("").is_none());
        assert!(query(&valid.replace(r#""@@//:type""#, r#""@@//:type"#)).is_none());
        assert!(query(&valid.replace(r#""@@//:type""#, r#""@@//:escaped\"name""#)).is_none());
        let absent = query(&valid.replace("  \"@@//:reader\"\n", "")).unwrap();
        assert!(!absent.contains_key("//:reader"));
    }
}
