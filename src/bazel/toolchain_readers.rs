//! Positive implementation edges for ordinary readers that Bazel resolves at analysis time.
use super::{
    canonical_query_label, query_set, target_label, target_name, BazelOptions, BazelVersion, Target,
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

type RepositoryRows<'a> = (
    &'a [Target],
    &'a [(Option<super::RepositoryInputs>, Option<Vec<String>>)],
);
type Bindings = BTreeMap<String, BTreeSet<String>>;

struct OrdinaryGraph<'a> {
    mapping: HashMap<String, String>,
    fine: BTreeSet<String>,
    rows: HashMap<String, &'a Target>,
    parents: HashMap<String, Vec<String>>,
    primary: BTreeSet<String>,
}

impl<'a> OrdinaryGraph<'a> {
    fn new(
        bazel: &BazelOptions,
        targets: &'a [Target],
        primary_len: usize,
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
            primary: BTreeSet::new(),
        };
        for (index, target) in targets.iter().enumerate() {
            let Some(name) = target_name(target) else {
                continue;
            };
            let id = graph.canonical(name);
            if id.starts_with("//external:") {
                continue;
            }
            if index < primary_len && id.starts_with("//") && target.rule.is_set() {
                graph.primary.insert(id.clone());
            }
            graph.rows.insert(id.clone(), target);
            for child in super::target_inputs(target) {
                graph
                    .parents
                    .entry(graph.canonical(child))
                    .or_default()
                    .push(id.clone());
            }
        }
        graph
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

/// A regular MODULE dependency remains an input even when an inferred edge has the same label.
pub(super) fn removed_membership(
    label: &str,
    ordinary: Option<&[String]>,
    mapping: &HashMap<String, String>,
    usages: &BTreeSet<String>,
) -> bool {
    let Some(name) = label.strip_prefix("//external:") else {
        return false;
    };
    let canonical = mapping.get(name).map_or(name, String::as_str);
    super::extension_usage_prefix(canonical).is_some_and(|usage| usages.contains(usage))
        && !ordinary.is_some_and(|inputs| inputs.iter().any(|input| input == label))
}

/// Every path lost by removing a membership has a first removed edge. Compute its
/// possible predecessors on the repository nodes actually returned by the lowerer.
fn affected_repositories(
    (rows, metadata): RepositoryRows<'_>,
    mapping: &HashMap<String, String>,
    usages: &BTreeSet<String>,
) -> HashSet<String> {
    let mut parents = HashMap::<&str, Vec<&str>>::new();
    let mut groups = HashSet::new();
    let mut affected = HashSet::new();
    for (row, (shared, ordinary)) in rows.iter().zip(metadata) {
        let rule = row.rule.as_option().expect("repository");
        let name = rule.name.as_str();
        for child in &rule.rule_input {
            if removed_membership(child, ordinary.as_deref(), mapping, usages) {
                affected.insert(name);
            } else {
                parents.entry(child).or_default().push(name);
            }
        }
        if let Some(group) = shared.as_ref().and_then(super::RepositoryInputs::siblings) {
            if usages.contains(&group.extension) {
                if !group.labels.is_empty() {
                    affected.insert(name);
                }
            } else {
                let key = group.extension.as_str();
                parents.entry(key).or_default().push(name);
                if groups.insert(key) {
                    for child in &group.labels {
                        parents.entry(child).or_default().push(key);
                    }
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

impl BazelOptions {
    pub(super) fn observe_external_inputs(
        &self,
        targets: &[Target],
        primary_len: usize,
        mapping: &[u8],
        survey: Option<&super::ExtensionRepositories>,
        repository_rows: RepositoryRows<'_>,
        version: Option<BazelVersion>,
    ) -> Result<ExtensionInputs> {
        let graph = OrdinaryGraph::new(self, targets, primary_len, mapping);
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
        let (bindings, covered) =
            options.resolved_toolchain_inputs(&graph, targets, mapping, version)?;
        let mut result = ExtensionInputs::default();
        if let Some(repositories) = survey.filter(|_| covered) {
            match options.extension_inputs(&graph, repositories, repository_rows, version) {
                Ok(observed) => result = observed,
                Err(error) => {
                    self.check_cancelled()?;
                    eprintln!("[Warn] retaining inferred external dependencies: {error:#}");
                }
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
    ) -> Result<Bindings> {
        let mut inputs = Bindings::new();
        for row in self.query_with(query, true, Some(version), false, &mut |_, _| {})? {
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
            }
        }
        Ok(inputs)
    }

    fn readers_of(
        &self,
        graph: &OrdinaryGraph<'_>,
        candidates: &BTreeSet<String>,
        types: &BTreeSet<String>,
        version: BazelVersion,
    ) -> Result<BTreeMap<String, String>> {
        if types.is_empty() || candidates.is_empty() {
            return Ok(BTreeMap::new());
        }
        let query = format!(
            "let candidates = {} in rdeps($candidates, {}, 1) intersect $candidates",
            query_set(candidates)?,
            query_set(types)?
        );
        let rows = self.query_with(
            &self.with_exclude_filter(query),
            false,
            Some(version),
            false,
            &mut |_, _| {},
        )?;
        Ok(rows
            .iter()
            .filter_map(target_name)
            .map(|name| (graph.canonical(name), name.to_owned()))
            .collect())
    }

    fn resolved_toolchain_inputs(
        &self,
        graph: &OrdinaryGraph<'_>,
        targets: &[Target],
        root_mapping: &[u8],
        version: BazelVersion,
    ) -> Result<(Bindings, bool)> {
        let fine = graph
            .rows
            .keys()
            .chain(graph.parents.keys())
            .filter(|name| repository(name).is_some_and(|repo| graph.fine.contains(repo)))
            .cloned()
            .collect();
        let reached = graph.ancestors(fine, |_| true);
        if graph.primary.is_empty() || reached.is_empty() {
            return Ok((Bindings::new(), true));
        }
        let observations = self.observed_toolchains(targets, Some(root_mapping), false)?;
        let mut implementations = BTreeMap::new();
        let mut types = BTreeSet::new();
        for declaration in observations.declarations.values() {
            for name in declaration
                .implementations
                .iter()
                .filter(|name| reached.contains(*name))
            {
                if let Some(actual) = graph.rows.get(name).and_then(|t| target_name(t)) {
                    implementations.insert(name.clone(), actual.to_owned());
                    types.extend(declaration.types.iter().cloned());
                }
            }
        }
        if types.is_empty() {
            return Ok((Bindings::new(), true));
        }
        let mut pending = types.clone();
        while !pending.is_empty() {
            let aliases = self.alias_actuals(&pending, version)?;
            pending.clear();
            for actual in aliases.values().flat_map(super::scalar_label_values) {
                let target = graph.canonical(actual);
                if types.insert(target.clone()) {
                    pending.insert(target);
                }
            }
        }
        let names = self.readers_of(graph, &graph.primary, &types, version)?;
        if names.is_empty() {
            return Ok((Bindings::new(), true));
        }
        // This observation only adds real positive edges. A shared toolchain can be
        // forced onto different execution platforms with the same public checksum.
        // No whole-closure identity or negative follows from a reader's direct row.
        let readers = names.values().cloned().collect();
        let mut configured = self.configured_children(graph, &query_set(&readers)?, version)?;
        let covered = names.keys().all(|name| configured.contains_key(name));
        let mut bindings = Bindings::new();
        for (id, name) in names {
            if let Some(inputs) = configured.remove(&id) {
                let suppliers: BTreeSet<_> = inputs
                    .iter()
                    .filter_map(|child| implementations.get(child).cloned())
                    .collect();
                if !suppliers.is_empty() {
                    bindings.insert(name, suppliers);
                }
            }
        }
        anyhow::ensure!(
            !bindings.is_empty(),
            "no resolved implementation on the nominated readers"
        );
        Ok((bindings, covered))
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
        graph: &OrdinaryGraph<'_>,
        repositories: &super::ExtensionRepositories,
        repository_rows: RepositoryRows<'_>,
        version: BazelVersion,
    ) -> Result<ExtensionInputs> {
        let usages: BTreeSet<_> = graph
            .fine
            .intersection(&repositories.generated)
            .filter_map(|repo| super::extension_usage_prefix(repo))
            .collect();
        let usage = |name: &str| {
            let family = super::extension_usage_prefix(repository(name)?)?;
            usages.get(family).map(|name| (*name).to_owned())
        };
        let canonical = |label: &str| graph.canonical(label);
        let mut result = ExtensionInputs::default();
        if usages.is_empty() {
            return Ok(result);
        }
        let mut roots = BTreeSet::new();
        let mut pending = Vec::new();
        for (child, callers) in &graph.parents {
            if usage(child).is_none() {
                continue;
            }
            for parent in callers
                .iter()
                .filter(|parent| graph.primary.contains(*parent))
            {
                roots.insert(parent.clone());
                pending.push(child.clone());
            }
        }
        let mut seen = BTreeSet::new();
        let mut readers = BTreeSet::new();
        let mut incomplete = BTreeSet::new();
        while let Some(name) = pending.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(family) = usage(&name) else { continue };
            let Some(row) = graph.rows.get(&name) else {
                incomplete.insert(family);
                continue;
            };
            result.usages.insert(family);
            if let Some(rule) = row.rule.as_option() {
                if rule.rule_class != "alias" {
                    readers.insert(name);
                }
                pending.extend(
                    rule.rule_input
                        .iter()
                        .map(|child| canonical(child))
                        .filter(|key| usage(key).is_some()),
                );
            } else if let Some(file) = row.generated_file.as_option() {
                pending.push(canonical(&file.generating_rule));
            }
        }
        result.usages.retain(|family| !incomplete.contains(family));
        if result.usages.is_empty() {
            return Ok(result);
        }
        // The direct consumer may itself be reached through a transition. Analyze the
        // callers that lead to it as well, asking Bazel to emit only the reached rows.
        let affected = affected_repositories(repository_rows, &graph.mapping, &result.usages);
        let nodes = crate::hash::canonical_repo_nodes(&graph.mapping);
        let companion = |label: &str| {
            let target = crate::hash::transform_external_input(
                label,
                &self.fine_grained_external_repos,
                &nodes,
            );
            affected.contains(target.as_ref())
        };
        let mut entries = BTreeSet::new();
        let mut boundary = readers;
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
        roots.extend(graph.primary.intersection(&possible).cloned());
        let roots = graph.ancestors(roots, |name| graph.primary.contains(name));
        let callers = query_set(&roots)?;
        // A compiler can come from another extension or be selected by a transitioned
        // external reader whose rule was never in the ordinary graph.
        let scope = format!("deps({callers})");
        let output = self.query_formatted(&scope,
            "def format(target):\n    return str(target.label) if 'ToolchainInfo' in (providers(target) or {}) else ''\n", version)?;
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
        let configured = self.configured_children(graph, &expression, version)?;
        if !roots.iter().all(|root| configured.contains_key(root)) {
            return Ok(ExtensionInputs::default());
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
                match graph.rows.get(child).and_then(|row| target_name(row)) {
                    Some(spelling) => {
                        inputs.insert(spelling.to_owned());
                    }
                    None => {
                        missing.insert(child.clone());
                        inputs.insert(child.clone());
                    }
                }
            }
            if !inputs.is_empty() {
                let spelling = graph
                    .rows
                    .get(name)
                    .and_then(|row| target_name(row))
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        missing.insert(name.clone());
                        name.clone()
                    });
                result.bindings.insert(spelling, inputs);
            }
        }
        if !missing.is_empty() {
            let expression = format!(
                "let d = deps({}) in $d + buildfiles($d)",
                query_set(&missing)?
            );
            result.sources =
                self.query_with(&expression, false, Some(version), false, &mut |_, _| {})?;
            let spellings: HashMap<_, _> = result
                .sources
                .iter()
                .filter_map(target_name)
                .map(|name| (canonical(name), name.to_owned()))
                .collect();
            if missing.iter().any(|name| !spellings.contains_key(name)) {
                result.usages.clear();
            }
            result.bindings = result
                .bindings
                .into_iter()
                .map(|(name, children)| {
                    let children = children
                        .into_iter()
                        .map(|name| spellings.get(&name).cloned().unwrap_or(name))
                        .collect();
                    (spellings.get(&name).cloned().unwrap_or(name), children)
                })
                .collect();
        }
        Ok(result)
    }
}
