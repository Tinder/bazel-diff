//! Positive implementation edges for ordinary readers that Bazel resolves at analysis time.
use super::{canonical_query_label, query_set, target_name, BazelOptions, BazelVersion, Target};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

type Bindings = BTreeMap<String, BTreeSet<String>>;

#[derive(Default)]
struct RegisteredToolchains {
    declarations: Vec<Target>,
    order: Vec<(String, BTreeSet<String>)>,
}

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
    coarse: HashMap<&'a str, Option<[u8; 32]>>,
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
        repositories: &'a [super::RepositoryRow],
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
            coarse: repositories
                .iter()
                .filter(|row| row.remote_fetch)
                .filter_map(|row| target_name(&row.target).map(|name| (name, row.setup_hash)))
                .collect(),
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
        let mut recovered = Vec::new();
        let mut graph = OrdinaryGraph::new(self, targets, primary, mapping, repository_rows);
        if graph.primary.is_empty() && repository_rows.iter().all(|row| row.memberships.is_empty())
        {
            return Ok(ExtensionInputs::default());
        }
        let Some(registrations) = options.registered_toolchains(version, &graph.mapping)? else {
            // MODULE dependencies may register a toolchain that never occurs in
            // this ordinary query. Keep their legacy repository inputs when the
            // producer cannot report the registrations.
            let mut result = ExtensionInputs::default();
            (result.bindings, result.sources) = options.supplier_inputs(
                &graph,
                mapping,
                version,
                &RegisteredToolchains::default(),
                &mut result,
                &mut BTreeSet::new(),
            )?;
            return Ok(result);
        };
        let mut opaque = Vec::new();
        if let Some(repositories) = survey {
            let families = graph
                .fine
                .intersection(&repositories.generated)
                .filter_map(|name| super::extension_usage_prefix(name))
                .map(str::to_owned)
                .collect();
            let affected = affected_repositories(repository_rows, &families);
            let nodes = crate::hash::canonical_repo_nodes(&graph.mapping);
            let is_boundary = |name: &str| {
                let companion = crate::hash::transform_external_input(
                    name,
                    &self.fine_grained_external_repos,
                    &nodes,
                );
                affected.contains(companion.as_ref())
                    || repository(name)
                        .and_then(super::extension_usage_prefix)
                        .is_some_and(|prefix| families.contains(prefix))
            };
            let mut pending: BTreeSet<_> = graph
                .parents
                .keys()
                .filter(|name| {
                    repository(name).is_some()
                        && is_boundary(name)
                        && !graph.rows.contains_key(*name)
                })
                .cloned()
                .collect();
            let mut requested = BTreeSet::new();
            let mut alias_rows = HashMap::new();
            let mut alias_packages = BTreeSet::new();
            let mut new_packages = BTreeSet::new();
            while !pending.is_empty() {
                let names = std::mem::take(&mut pending);
                requested.extend(names.iter().cloned());
                // buildfiles($d) must cover every package whose aliases we reuse.
                // A queued input might already have arrived as a BUILD or load row.
                new_packages.retain(|package: &String| {
                    names
                        .range(format!("{package}:")..)
                        .next()
                        .and_then(|name| super::external_query_package(name))
                        == Some(package.as_str())
                });
                let rows = options.ordinary_rows(&names, version, false, Some(&new_packages))?;
                new_packages.clear();
                let mut reached = std::collections::VecDeque::new();
                for row in rows {
                    // The package inventory contains aliases only. Admitting an
                    // unrequested alias here would also admit its unused inputs.
                    if let Some(rule) = row
                        .rule
                        .as_option()
                        .filter(|rule| rule.rule_class == "alias")
                    {
                        let name = graph.canonical(&rule.name);
                        if !names.contains(&name) {
                            alias_rows.insert(name, row);
                            continue;
                        }
                    }
                    reached.push_back(row);
                }
                while let Some(row) = reached.pop_front() {
                    if let Some(name) = target_name(&row) {
                        requested.insert(graph.canonical(name));
                    }
                    let alias = row
                        .rule
                        .as_option()
                        .filter(|rule| rule.rule_class == "alias")
                        .map(|rule| graph.canonical(&rule.name));
                    for child in super::target_inputs(&row).map(|name| graph.canonical(name)) {
                        if repository(&child).is_some()
                            && is_boundary(&child)
                            && !graph.rows.contains_key(&child)
                            && !requested.contains(&child)
                        {
                            if let Some(cached) = alias_rows.remove(&child) {
                                requested.insert(child);
                                reached.push_back(cached);
                            } else {
                                if let Some(package) = super::external_query_package(&child) {
                                    if alias.as_deref().and_then(super::external_query_package)
                                        == Some(package)
                                        && alias_packages.insert(package.to_owned())
                                    {
                                        new_packages.insert(package.to_owned());
                                    }
                                }
                                pending.insert(child);
                            }
                        }
                    }
                    opaque.push(row);
                }
                pending.retain(|name| !requested.contains(name));
            }
            let entries: Vec<_> = graph
                .parents
                .keys()
                .filter(|name| repository(name).is_some() && is_boundary(name))
                .cloned()
                .collect();
            graph.index(&opaque);
            for name in entries {
                if graph.rows.get(&name).is_some_and(|row| row.rule.is_set()) {
                    graph.primary.entry(name).or_insert(None);
                }
            }
        }
        let mut result = match survey {
            Some(repositories) => options.extension_inputs(
                &mut graph,
                repositories,
                repository_rows,
                version,
                &mut recovered,
            )?,
            None => ExtensionInputs::default(),
        };
        let mut unresolved = BTreeSet::new();
        let (bindings, suppliers) = options.supplier_inputs(
            &graph,
            mapping,
            version,
            &registrations,
            &mut result,
            &mut unresolved,
        )?;
        for family in &result.usages {
            retain_memberships(
                &mut result.bindings,
                repository_rows,
                family,
                unresolved.clone(),
            );
        }
        for (reader, inputs) in bindings {
            result.bindings.entry(reader).or_default().extend(inputs);
        }
        drop(graph);
        recovered.extend(suppliers);
        recovered.extend(opaque);
        result.sources = super::deduplicate_targets(recovered)?;
        Ok(result)
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
                        eprintln!(
                            "[Warn] retaining possible toolchain implementations for {} rules represented by {representative}",
                            members.len()
                        );
                        readers
                            .unknown
                            .extend(members.iter().map(|name| (*name).to_owned()));
                    }
                }
            }
        }
        if self.verbose {
            eprintln!(
                "[Info] Toolchain types examined {} main rules in {} class observations: {} nominated, {} unresolved",
                graph.primary.len(),
                representatives.len(),
                readers.known.len(),
                readers.unknown.len()
            );
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

    fn registered_toolchains(
        &self,
        version: BazelVersion,
        mapping: &HashMap<String, String>,
    ) -> Result<Option<RegisteredToolchains>> {
        let output = self.run_capture(&[
            "mod",
            "graph",
            "--output=json",
            "--include_toolchain_registrations",
        ])?;
        if !output.status.success() {
            self.check_cancelled()?;
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            if self.verbose
                || !diagnostic.contains("Unrecognized option: --include_toolchain_registrations")
            {
                eprintln!("[Warn] retaining repository inputs because MODULE registration lookup failed ({}): {}", output.status, diagnostic.trim());
            }
            return Ok(None);
        }
        #[derive(serde::Deserialize)]
        struct RegistrationGraph {
            #[serde(rename = "moduleToolchainRegistrations")]
            registrations: Option<Vec<String>>,
        }
        let Some(registrations) =
            serde_json::from_slice::<RegistrationGraph>(&output.stdout)?.registrations
        else {
            return Ok(None);
        };
        let mut metadata = self.clone();
        metadata.command_options.extend(
            [
                "--proto:rule_classes",
                "--noproto:flatten_selects",
                "--proto:output_rule_attrs=actual,toolchain,toolchain_type",
                "--consistent_labels",
            ]
            .map(str::to_owned),
        );
        let canonical = |label: &str| canonical_query_label(label, mapping).into_owned();
        let registrations: Vec<_> = registrations.iter().map(|label| canonical(label)).collect();
        let mut pending: BTreeSet<_> = registrations.iter().cloned().collect();
        let mut aliases = Bindings::new();
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        while !pending.is_empty() {
            let names = std::mem::take(&mut pending);
            let rows = metadata.query_with(
                &query_set(&names)?,
                false,
                Some(version),
                false,
                &mut |_, _| {},
            )?;
            seen.extend(names);
            for row in rows {
                if let Some(rule) = row.rule.as_option() {
                    match rule.rule_class_key.as_deref() {
                        Some("alias") => {
                            for name in rule
                                .attribute
                                .iter()
                                .filter(|a| a.name == "actual")
                                .flat_map(super::scalar_label_values)
                            {
                                let name = canonical(name);
                                aliases
                                    .entry(canonical(&rule.name))
                                    .or_default()
                                    .insert(name.clone());
                                if !seen.contains(&name) {
                                    pending.insert(name);
                                }
                            }
                        }
                        Some("toolchain") => result.push(row),
                        _ => {}
                    }
                }
            }
        }
        let order: Vec<_> = registrations
            .into_iter()
            .map(|registration| {
                let mut pending = vec![registration.clone()];
                let mut visited = BTreeSet::new();
                let mut terminals = BTreeSet::new();
                while let Some(label) = pending.pop() {
                    if !visited.insert(label.clone()) {
                        continue;
                    }
                    if let Some(targets) = aliases.get(&label) {
                        pending.extend(targets.iter().cloned());
                    } else {
                        terminals.insert(label);
                    }
                }
                (registration, terminals)
            })
            .collect();
        let declarations: HashSet<_> = result
            .iter()
            .filter_map(|row| row.rule.as_option())
            .filter(|rule| {
                ["toolchain", "toolchain_type"].iter().all(|key| {
                    rule.attribute
                        .iter()
                        .filter(|attr| &attr.name == key)
                        .flat_map(super::scalar_label_values)
                        .any(|label| label.starts_with("//") || label.starts_with('@'))
                })
            })
            .map(|rule| canonical(&rule.name))
            .collect();
        if let Some((label, _)) = order.iter().find(|(_, terminals)| {
            terminals.is_empty()
                || terminals
                    .iter()
                    .any(|terminal| !declarations.contains(terminal.as_str()))
        }) {
            eprintln!("[Warn] retaining repository inputs because a registered target did not resolve to a toolchain declaration: {label}");
            return Ok(None);
        }
        Ok(Some(RegisteredToolchains {
            declarations: result,
            order,
        }))
    }

    fn supplier_inputs(
        &self,
        graph: &OrdinaryGraph<'_>,
        root_mapping: &[u8],
        version: BazelVersion,
        registrations: &RegisteredToolchains,
        result: &mut ExtensionInputs,
        unresolved: &mut BTreeSet<String>,
    ) -> Result<(Bindings, Vec<Target>)> {
        let fine = graph
            .rows
            .keys()
            .chain(graph.parents.keys())
            .filter(|name| repository(name).is_some_and(|repo| graph.fine.contains(repo)))
            .cloned()
            .collect();
        let reached = graph.ancestors(fine, |_| true);
        if graph.primary.is_empty() || reached.is_empty() {
            return Ok((Bindings::new(), Vec::new()));
        }
        let observations = self.observed_toolchain_rows(
            graph.rows.values().copied(),
            &registrations.declarations,
            Some(root_mapping),
            false,
        )?;
        let mut implementations = BTreeMap::new();
        let mut missing = BTreeSet::new();
        let mut by_type = Bindings::new();
        let mut predicates = Bindings::new();
        for (label, declaration) in &observations.declarations {
            for name in declaration.implementations.iter().filter(|name| {
                reached.contains(*name)
                    || (!registrations.declarations.is_empty() && !graph.rows.contains_key(*name))
            }) {
                let actual = graph
                    .rows
                    .get(name)
                    .and_then(|row| target_name(row))
                    .unwrap_or(name);
                if !graph.rows.contains_key(name) {
                    missing.insert(name.clone());
                }
                implementations.insert(name.clone(), actual.to_owned());
                for ty in &declaration.types {
                    predicates
                        .entry(ty.clone())
                        .or_default()
                        .insert(label.clone());
                    by_type
                        .entry(ty.clone())
                        .or_default()
                        .insert(actual.to_owned());
                }
            }
        }
        // A module can register targets in two other repositories. Their declarations,
        // implementations and MODULE files can all remain unchanged when priority flips.
        let mut ranks = BTreeMap::<String, BTreeSet<(usize, String, String)>>::new();
        for (index, (registration, choices)) in registrations.order.iter().enumerate() {
            for terminal in choices {
                if let Some(rule) = observations.declarations.get(&graph.canonical(terminal)) {
                    for ty in &rule.types {
                        predicates
                            .entry(ty.clone())
                            .or_default()
                            .insert(registration.clone());
                        ranks.entry(ty.clone()).or_default().insert((
                            index,
                            registration.clone(),
                            terminal.clone(),
                        ));
                    }
                }
            }
        }
        let mut pending: BTreeSet<_> = by_type.keys().cloned().collect();
        if pending.is_empty() {
            return Ok((Bindings::new(), Vec::new()));
        }
        while !pending.is_empty() {
            let aliases = self.alias_actuals(&pending, version)?;
            pending.clear();
            for (alias, actual) in aliases {
                let alias = graph.canonical(&alias);
                let inputs = by_type[&alias].clone();
                let order = ranks.get(&alias).cloned().unwrap_or_default();
                let declarations = predicates.get(&alias).cloned().unwrap_or_default();
                for label in super::scalar_label_values(&actual) {
                    let target = graph.canonical(label);
                    let roles = predicates.entry(target.clone()).or_default();
                    let roles_before = roles.len();
                    roles.extend(declarations.iter().cloned());
                    let roles_changed = roles.len() != roles_before;
                    let current = ranks.entry(target.clone()).or_default();
                    let order_before = current.len();
                    current.extend(order.iter().cloned());
                    let order_changed = order_before != current.len();
                    let candidates = by_type.entry(target.clone()).or_default();
                    let before = candidates.len();
                    candidates.extend(inputs.iter().cloned());
                    if candidates.len() != before || order_changed || roles_changed {
                        pending.insert(target);
                    }
                }
            }
        }
        let types = by_type.keys().cloned().collect();
        let rank_hashes: BTreeMap<_, _> = ranks
            .into_iter()
            .map(|(ty, enrollment)| {
                let mut hash = Sha256::new();
                for (_, alias, terminal) in enrollment {
                    hash.update((alias.len() as u64).to_le_bytes());
                    hash.update(alias.as_bytes());
                    hash.update((terminal.len() as u64).to_le_bytes());
                    hash.update(terminal.as_bytes());
                }
                (ty, <[u8; 32]>::from(hash.finalize()))
            })
            .collect();
        let fingerprint = |types: &BTreeSet<String>| {
            let mut values = types.iter().filter_map(|ty| rank_hashes.get(ty)).peekable();
            values.peek()?;
            let mut hash = Sha256::new();
            for digest in values {
                hash.update(digest);
            }
            Some(<[u8; 32]>::from(hash.finalize()))
        };
        let readers = self.readers_of(graph, &types)?;
        let mut bindings = Bindings::new();
        let mut missing_declarations = BTreeSet::new();
        for (id, types) in readers.known {
            let rule = graph.rows[&id].rule.as_option().expect("nominated rule");
            if matches!(
                rule.rule_class_key.as_deref(),
                Some("toolchain" | "toolchain_type" | "alias")
            ) {
                continue;
            }
            if let Some(order) = fingerprint(&types) {
                result.registration_hashes.insert(rule.name.clone(), order);
            }
            let inputs = bindings.entry(rule.name.clone()).or_default();
            inputs.extend(types.iter().flat_map(|ty| by_type[ty].iter().cloned()));
            // Eligibility can change while registration order, type and actual
            // implementation labels stay fixed. Hash the declared predicates
            // and the forwarding alias as ordinary target definitions.
            result
                .declarations
                .entry(rule.name.clone())
                .or_default()
                .extend(
                    types
                        .iter()
                        .filter_map(|ty| predicates.get(ty))
                        .flatten()
                        .cloned(),
                );
        }
        let demanded: BTreeSet<_> = result.declarations.values().flatten().collect();
        for label in demanded {
            if let Some(row) = graph.rows.get(label) {
                missing_declarations.extend(
                    super::target_inputs(row)
                        .map(|child| graph.canonical(child))
                        .filter(|child| !graph.rows.contains_key(child)),
                );
            } else {
                missing_declarations.insert(label.clone());
            }
        }
        // An unavailable ordinary reader does not establish demand for every
        // toolchain installed by Bazel. Keep the known fine suppliers and the
        // implementations already observed for the other readers.
        let mut possible: BTreeSet<_> = bindings.values().flatten().cloned().collect();
        possible.extend(
            implementations
                .iter()
                .filter(|(name, _)| reached.contains(*name))
                .map(|(_, spelling)| spelling.clone()),
        );
        let possible_order = (!readers.unknown.is_empty())
            .then(|| fingerprint(&types))
            .flatten();
        for name in readers.unknown {
            let spelling = target_name(graph.rows[&name]).expect("primary rule");
            unresolved.insert(spelling.to_owned());
            if let Some(hash) = possible_order {
                result.registration_hashes.insert(spelling.to_owned(), hash);
            }
            bindings
                .entry(spelling.to_owned())
                .or_default()
                .extend(possible.iter().cloned());
        }
        let used: BTreeSet<_> = bindings.values().flatten().cloned().collect();
        missing.retain(|name| used.contains(name));
        // The ordinary external-repository contract already represents separately
        // fetched payloads by their repository definition. Preserve each demanded
        // supplier's own rules, and follow fine-grained, local and otherwise
        // unrepresented inputs. A recursive deps() here fetches SDKs for every
        // platform merely to discover inputs of a small number of wrappers.
        let owners: HashSet<_> = missing
            .iter()
            .filter_map(|name| repository(name))
            .map(str::to_owned)
            .collect();
        let fine_families: HashSet<_> = graph
            .fine
            .iter()
            .filter_map(|name| super::extension_usage_prefix(name))
            .collect();
        let nodes = crate::hash::canonical_repo_nodes(&graph.mapping);
        let mut pending = missing;
        pending.extend(missing_declarations);
        let mut visited = HashSet::new();
        let mut sources = Vec::new();
        let mut setups = BTreeMap::<String, BTreeSet<[u8; 32]>>::new();
        while !pending.is_empty() {
            let names = std::mem::take(&mut pending);
            visited.extend(names.iter().cloned());
            let absent = names
                .iter()
                .filter(|name| !graph.rows.contains_key(*name))
                .cloned()
                .collect();
            let rows = self.ordinary_rows(&absent, version, false, None)?;
            let existing = names
                .iter()
                .filter_map(|name| graph.rows.get(name).copied());
            for row in existing.chain(rows.iter()) {
                if !target_name(row).is_some_and(|name| names.contains(&graph.canonical(name))) {
                    continue;
                }
                for child in super::target_inputs(row).map(|name| graph.canonical(name)) {
                    if visited.contains(&child) || child.starts_with("//external:") {
                        continue;
                    }
                    let boundary = repository(&child)
                        .filter(|repo| {
                            !owners.contains(*repo)
                                && !graph.fine.contains(*repo)
                                && !super::extension_usage_prefix(repo)
                                    .is_some_and(|family| fine_families.contains(family))
                        })
                        .and_then(|_| {
                            graph.coarse.get(
                                crate::hash::transform_external_input(
                                    &child,
                                    &self.fine_grained_external_repos,
                                    &nodes,
                                )
                                .as_ref(),
                            )
                        });
                    match boundary.filter(|_| !graph.rows.contains_key(&child)) {
                        Some(Some(digest)) => {
                            let name = target_name(row).expect("named supplier");
                            setups.entry(name.to_owned()).or_default().insert(*digest);
                        }
                        Some(None) => (),
                        None => {
                            pending.insert(child);
                        }
                    }
                }
            }
            sources.extend(rows);
        }
        for (supplier, inputs) in setups {
            let mut hash = Sha256::new();
            for input in inputs {
                hash.update(input);
            }
            result
                .supplier_hashes
                .insert(supplier, hash.finalize().into());
        }
        let spellings: HashMap<_, _> = sources
            .iter()
            .filter_map(target_name)
            .map(|name| (graph.canonical(name), name))
            .collect();
        for (reader, definitions) in &mut result.declarations {
            *definitions = std::mem::take(definitions)
                .into_iter()
                .filter_map(|label| {
                    let row = graph
                        .rows
                        .get(&label)
                        .and_then(|row| target_name(row))
                        .or_else(|| spellings.get(&label).copied());
                    if row.is_none() {
                        bindings.entry(reader.clone()).or_default().insert(label);
                    }
                    row.map(str::to_owned)
                })
                .collect();
        }
        for children in bindings.values_mut() {
            *children = std::mem::take(children)
                .into_iter()
                .map(|child| {
                    spellings
                        .get(&graph.canonical(&child))
                        .map_or(child, |name| (*name).to_owned())
                })
                .collect();
        }
        Ok((bindings, sources))
    }
}

#[derive(Default)]
pub(super) struct ExtensionInputs {
    pub(super) declarations: Bindings,
    pub(super) registration_hashes: BTreeMap<String, [u8; 32]>,
    pub(super) supplier_hashes: BTreeMap<String, [u8; 32]>,
    pub(super) usages: BTreeSet<String>,
    pub(super) bindings: BTreeMap<String, BTreeSet<String>>,
    pub(super) sources: Vec<Target>,
}

impl BazelOptions {
    /// Record ordinary edges into generated repositories; callers of a missing
    /// operand retain the repository memberships that used to cover that input.
    fn extension_inputs<'a>(
        &self,
        graph: &mut OrdinaryGraph<'a>,
        repositories: &super::ExtensionRepositories,
        repository_rows: &[super::RepositoryRow],
        version: BazelVersion,
        recovered: &'a mut Vec<Target>,
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
        let mut missing = visit(graph, direct.iter().cloned().collect());
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
            missing = visit(graph, missing.into_iter().collect());
        }
        // A target that names a removed alias can be present in an ordinary
        // wildcard query even though analyzing it would fail. Its readers
        // retain the membership candidates that covered the unavailable input.

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
        for name in readers {
            graph.primary.entry(name).or_insert(None);
        }
        Ok(result)
    }

    fn recover_ordinary_inputs(
        &self,
        names: &BTreeSet<String>,
        version: BazelVersion,
    ) -> Result<Vec<Target>> {
        self.ordinary_rows(names, version, true, None)
    }

    fn ordinary_rows(
        &self,
        names: &BTreeSet<String>,
        version: BazelVersion,
        recursive: bool,
        alias_packages: Option<&BTreeSet<String>>,
    ) -> Result<Vec<Target>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let set = query_set(names)?;
        let roots = if recursive {
            format!("deps({set})")
        } else {
            set
        };
        let expression = format!("let d = {roots} in $d + buildfiles($d)");
        let mut options = self.clone();
        // Like alias acquisition, a missing input can lead to an arbitrary
        // rule with concatenated dictionary selects.
        options.command_options.extend([
            "--keep_going".to_owned(),
            "--noproto:flatten_selects".to_owned(),
        ]);
        // A missing alias or inactive dependency can coexist with valid rule
        // and source rows. Missing operands retain their own fallback below.
        let query = |options: &Self, expression: &str| {
            let graph = options.query_with_capture(
                expression,
                false,
                Some(version),
                false,
                &mut |_, _| {},
                super::CqueryCapture::AvailableOrdinaryInputs,
                |_| true,
            )?;
            super::deduplicate_targets(graph.targets)
        };
        if let Some(packages) = alias_packages.filter(|packages| !packages.is_empty()) {
            let inventory = super::package_targets(packages)?;
            let combined = format!("{expression} + kind('^alias rule$', ({inventory}))");
            // Only the original literal lookup may supply a partial answer:
            // an error in the extra inventory must not suppress its BUILD or target rows.
            options.keep_going = false;
            match query(&options, &combined) {
                Ok(rows) => return Ok(rows),
                Err(error) => {
                    self.check_cancelled()?;
                    if self.verbose {
                        eprintln!(
                            "[Info] retrying ordinary inputs without package aliases: {error}"
                        );
                    }
                }
            }
        }
        options.keep_going = true;
        query(&options, &expression)
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
