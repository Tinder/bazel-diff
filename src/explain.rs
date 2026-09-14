//! `bazel-diff explain`: attributes an impacted target to the upstream
//! target(s) actually responsible for its hash change, and renders the
//! answer as text, JSON, Graphviz DOT or a Mermaid flowchart.
//!
//! Answers [issue #479](https://github.com/Tinder/bazel-diff/issues/479).
//! Pure post-processing over the same three files `get-impacted-targets`
//! consumes -- it runs no Bazel query and needs no workspace, so it is cheap
//! enough to run on a CI failure after the fact.

use crate::model::{impacted_types, ImpactType, TargetHash};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;

/// Why a root cause's own hash moved, independent of anything upstream of it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RootCauseKind {
    /// The label exists in the final revision but not in the starting one.
    NewTarget,
    /// The label exists in both revisions and its own `directHash` changed.
    SelfChanged,
}

/// One reason the queried target was impacted: a target whose *own* hash
/// changed ([`ImpactType::Direct`]) and from which the change propagates down
/// to the queried target.
///
/// `path` runs in impact-propagation order -- `[root, …, target]` -- so
/// reading it left to right follows the change as it flows downstream. It is
/// a shortest such path; there may be others of equal length, and
/// `target_distance` is its length in dependency hops.
///
/// `package_hops` counts how many of those hops cross a Bazel package boundary
/// *along `path`*. Deliberately not named `packageDistance`:
/// `/impacted_targets_with_distances` reports the *minimum* package distance
/// over all paths, which is independently minimised and can therefore be
/// smaller than the package crossings on this (target-distance-minimal) path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootCause {
    pub label: String,
    pub target_type: String,
    pub kind: RootCauseKind,
    pub target_distance: usize,
    pub package_hops: usize,
    pub path: Vec<String>,
}

/// A node of the rendered blame subgraph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainNode {
    pub label: String,
    pub target_type: String,
    pub impact_type: String,
    pub target_distance: usize,
    pub is_root_cause: bool,
    pub is_queried_target: bool,
}

/// An edge of the rendered blame subgraph, in impact-propagation direction:
/// `from` is a dependency of `to`, so a change in `from` flows into `to`. This
/// is the reverse of the `--depEdgesFile` orientation, which maps a label to
/// the deps it consumes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExplainEdge {
    pub from: String,
    pub to: String,
}

/// The answer to "why was this target impacted?".
///
/// `root_causes` is truncated to the caller's `max_roots`; `total_root_causes`
/// is the count before truncation so no cap is ever silently applied.
/// `nodes`/`edges` describe only the subgraph spanned by the *reported* root
/// causes' paths.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactExplanation {
    pub target: String,
    pub target_type: String,
    pub impacted: bool,
    /// True when the label exists in the starting revision but not the final
    /// one. bazel-diff never reports a deleted target as impacted (there is
    /// nothing left to build), so such a target is reported unimpacted -- but
    /// for a *different* reason than one whose hash simply did not move, and
    /// the two must not read the same.
    pub removed: bool,
    pub directly_changed: bool,
    pub root_causes: Vec<RootCause>,
    pub total_root_causes: usize,
    pub truncated: bool,
    pub nodes: Vec<ExplainNode>,
    pub edges: Vec<ExplainEdge>,
}

/// The queried label appears in neither revision's hash file -- almost always
/// a typo.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownTargetError(pub String);

impl fmt::Display for UnknownTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is not present in either hash file. Check the label spelling, and note that \
             hashes generated with --targetType only contain the types you asked for.",
            self.0
        )
    }
}

impl std::error::Error for UnknownTargetError {}

/// Bounds on the attribution walk. Neither ever truncates silently: the full
/// root-cause count is always reported, and a depth cut adds a warning.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExplainLimits {
    /// Stop the walk this many hops above the queried target; `None` is unbounded.
    pub max_depth: Option<usize>,
    /// Keep at most this many root causes (nearest first); `0` means all.
    pub max_roots: usize,
}

/// The explanation plus the diagnostics the walk produced. `warnings` are
/// conditions the user should see regardless of verbosity (a depth bound that
/// hid root causes); `notes` are informational (a root-cause cap that applied).
#[derive(Clone, Debug)]
pub struct ExplainOutcome {
    pub explanation: ImpactExplanation,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

/// Attributes an impacted target to the upstream target(s) actually
/// responsible for its hash change.
///
/// The traversal is a breadth-first walk *up* the dependency edges from the
/// queried target, confined to labels that are themselves impacted. Every
/// DIRECT label it reaches is a root cause, and the BFS tree yields a shortest
/// propagation path from each. Confining the walk to impacted labels is what
/// keeps it cheap on a monorepo graph: an unimpacted dep cannot have
/// contributed to a hash change, so the visited set is bounded by the impacted
/// subgraph reachable from one target rather than by the whole build graph.
///
/// Note the walk continues *through* DIRECT labels rather than stopping at the
/// first one. A target that changed itself may also have changed deps, and
/// both are genuine reasons the queried target moved -- exactly the "service A
/// can be triggered because of multiple reasons" case in the issue.
pub fn explain(
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
    dep_edges: &BTreeMap<String, Vec<String>>,
    target: &str,
    limits: ExplainLimits,
) -> Result<ExplainOutcome, UnknownTargetError> {
    if !to.contains_key(target) && !from.contains_key(target) {
        return Err(UnknownTargetError(target.to_owned()));
    }

    let target_type = type_of(target, to, from);
    let impacted = impacted_types(from, to);
    let mut warnings = Vec::new();
    let mut notes = Vec::new();

    if !impacted.contains_key(target) {
        return Ok(ExplainOutcome {
            explanation: ImpactExplanation {
                target: target.to_owned(),
                target_type,
                impacted: false,
                removed: !to.contains_key(target),
                directly_changed: false,
                root_causes: Vec::new(),
                total_root_causes: 0,
                truncated: false,
                nodes: Vec::new(),
                edges: Vec::new(),
            },
            warnings,
            notes,
        });
    }

    let (distances, parents, truncated_by_depth) =
        walk_upstream(target, dep_edges, &impacted, limits.max_depth);
    if truncated_by_depth {
        warnings.push(format!(
            "Stopped the search at --maxDepth={} hops above {target}; root causes further \
             upstream are not reported. Raise or drop --maxDepth for the complete attribution.",
            limits.max_depth.unwrap_or_default()
        ));
    }

    let mut all_roots = distances
        .iter()
        .filter(|(label, _)| impacted.get(label.as_str()) == Some(&ImpactType::Direct))
        .map(|(root, distance)| {
            let path = path_to_target(root, &parents);
            let package_hops = path
                .windows(2)
                .filter(|pair| package_of(&pair[0]) != package_of(&pair[1]))
                .count();
            RootCause {
                label: root.clone(),
                target_type: type_of(root, to, from),
                kind: if from.contains_key(root) {
                    RootCauseKind::SelfChanged
                } else {
                    RootCauseKind::NewTarget
                },
                target_distance: *distance,
                package_hops,
                path,
            }
        })
        .collect::<Vec<_>>();
    all_roots.sort_by(|a, b| {
        a.target_distance
            .cmp(&b.target_distance)
            .then_with(|| a.label.cmp(&b.label))
    });

    let total = all_roots.len();
    let kept = if limits.max_roots > 0 && all_roots.len() > limits.max_roots {
        all_roots.truncate(limits.max_roots);
        notes.push(format!(
            "Reporting {} of {total} root causes for {target} (--maxRootCauses); pass \
             --maxRootCauses=0 for all of them",
            all_roots.len()
        ));
        all_roots
    } else {
        all_roots
    };

    let (nodes, edges) = build_subgraph(&kept, target, &distances, &impacted, dep_edges, to, from);

    Ok(ExplainOutcome {
        explanation: ImpactExplanation {
            target: target.to_owned(),
            target_type,
            impacted: true,
            removed: false,
            directly_changed: impacted.get(target) == Some(&ImpactType::Direct),
            truncated: kept.len() < total,
            root_causes: kept,
            total_root_causes: total,
            nodes,
            edges,
        },
        warnings,
        notes,
    })
}

/// Breadth-first walk from `target` up the dependency edges, visiting only
/// impacted labels. Returns each visited label's hop distance from `target`,
/// its parent in the BFS tree (the label it was first reached from, i.e. one
/// hop *downstream* of it), and whether the depth bound hid anything.
#[allow(clippy::type_complexity)]
fn walk_upstream(
    target: &str,
    dep_edges: &BTreeMap<String, Vec<String>>,
    impacted: &HashMap<String, ImpactType>,
    max_depth: Option<usize>,
) -> (BTreeMap<String, usize>, HashMap<String, String>, bool) {
    let mut distances = BTreeMap::from([(target.to_owned(), 0)]);
    let mut parents = HashMap::new();
    let mut queue = VecDeque::from([target.to_owned()]);
    let mut truncated_by_depth = false;

    while let Some(current) = queue.pop_front() {
        let depth = distances[&current];
        let deps = dep_edges.get(&current).map(Vec::as_slice).unwrap_or(&[]);
        if max_depth.is_some_and(|max| depth >= max) {
            if deps
                .iter()
                .any(|dep| impacted.contains_key(dep) && !distances.contains_key(dep))
            {
                truncated_by_depth = true;
            }
            continue;
        }
        for dep in deps {
            if !impacted.contains_key(dep) || distances.contains_key(dep) {
                continue;
            }
            distances.insert(dep.clone(), depth + 1);
            parents.insert(dep.clone(), current.clone());
            queue.push_back(dep.clone());
        }
    }

    (distances, parents, truncated_by_depth)
}

/// Unrolls the BFS parent chain into a `[root, …, target]` propagation path.
fn path_to_target(root: &str, parents: &HashMap<String, String>) -> Vec<String> {
    let mut path = vec![root.to_owned()];
    let mut cursor = root;
    while let Some(parent) = parents.get(cursor) {
        path.push(parent.clone());
        cursor = parent;
    }
    path
}

/// Builds the subgraph spanned by the reported `roots`' paths. Nodes are
/// exactly the labels on those paths; edges are *every* dependency edge
/// between two included nodes, not just the path edges, so the rendered graph
/// shows the real connectivity rather than a spanning tree.
fn build_subgraph(
    roots: &[RootCause],
    target: &str,
    distances: &BTreeMap<String, usize>,
    impacted: &HashMap<String, ImpactType>,
    dep_edges: &BTreeMap<String, Vec<String>>,
    to: &BTreeMap<String, TargetHash>,
    from: &BTreeMap<String, TargetHash>,
) -> (Vec<ExplainNode>, Vec<ExplainEdge>) {
    let root_labels = roots
        .iter()
        .map(|root| root.label.as_str())
        .collect::<HashSet<_>>();
    let mut included = roots
        .iter()
        .flat_map(|root| root.path.iter().cloned())
        .collect::<BTreeSet<_>>();
    included.insert(target.to_owned());

    let mut nodes = included
        .iter()
        .map(|label| ExplainNode {
            label: label.clone(),
            target_type: type_of(label, to, from),
            impact_type: match impacted[label] {
                ImpactType::Direct => "direct".to_owned(),
                ImpactType::Indirect => "indirect".to_owned(),
            },
            target_distance: distances[label],
            is_root_cause: root_labels.contains(label.as_str()),
            is_queried_target: label == target,
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|a, b| {
        b.target_distance
            .cmp(&a.target_distance)
            .then_with(|| a.label.cmp(&b.label))
    });

    let edges = included
        .iter()
        .flat_map(|consumer| {
            dep_edges
                .get(consumer)
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .filter(|dep| included.contains(*dep))
                .map(|dep| ExplainEdge {
                    from: dep.clone(),
                    to: consumer.clone(),
                })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    (nodes, edges)
}

impl PartialOrd for ExplainEdge {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExplainEdge {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.from
            .cmp(&other.from)
            .then_with(|| self.to.cmp(&other.to))
    }
}

/// The target's declared type, preferring the final revision. Empty when
/// types were not hashed.
fn type_of(
    label: &str,
    to: &BTreeMap<String, TargetHash>,
    from: &BTreeMap<String, TargetHash>,
) -> String {
    to.get(label)
        .filter(|hash| !hash.kind.is_empty())
        .or_else(|| from.get(label).filter(|hash| !hash.kind.is_empty()))
        .map(|hash| hash.kind.clone())
        .unwrap_or_default()
}

/// The package part of a label, i.e. everything before the `:`.
fn package_of(label: &str) -> &str {
    label.split_once(':').map_or(label, |(package, _)| package)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Output shapes supported by `bazel-diff explain`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExplainFormat {
    Text,
    Json,
    Dot,
    Mermaid,
}

// Categorical slots 1 and 2 of the validated light-mode palette, plus a
// deliberately recessive neutral for intermediates (a pass-through node is
// context, not a peer category). Node identity is carried by the stroke; the
// fill is one uniform near-white surface so the output reads the same on any
// page background (DOT and Mermaid are static source rendered elsewhere), and
// color is never the sole encoding: roles also differ in shape/weight and are
// labelled directly.
const SURFACE: &str = "#fcfcfb";
const INK: &str = "#0b0b0b";
const INK_MUTED: &str = "#52514e";
const HUE_ROOT_CAUSE: &str = "#eb6834"; // orange -- where the change originates
const HUE_QUERIED: &str = "#2a78d6"; // blue -- what the user asked about
const HUE_INTERMEDIATE: &str = "#6b6a66"; // neutral -- carries the change through

/// Renders an explanation in the requested format. Every format ends with a
/// newline.
pub fn render(explanation: &ImpactExplanation, format: ExplainFormat) -> String {
    match format {
        ExplainFormat::Text => render_text(explanation),
        ExplainFormat::Json => {
            let mut json =
                serde_json::to_string_pretty(explanation).expect("explanation serializes to JSON");
            json.push('\n');
            json
        }
        ExplainFormat::Dot => render_dot(explanation),
        ExplainFormat::Mermaid => render_mermaid(explanation),
    }
}

fn type_suffix(target_type: &str) -> String {
    if target_type.is_empty() {
        String::new()
    } else {
        format!("  [{target_type}]")
    }
}

fn render_text(e: &ImpactExplanation) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}{}\n", e.target, type_suffix(&e.target_type)));
    if !e.impacted {
        out.push_str(if e.removed {
            "NOT IMPACTED -- this target exists in the starting revision but not the final \
             one. Deleted targets are never reported as impacted; there is nothing left \
             to build.\n"
        } else {
            "NOT IMPACTED -- its hash is identical between the two revisions.\n"
        });
        return out;
    }

    out.push_str("IMPACTED ");
    out.push_str(if e.directly_changed {
        "(directly -- this target changed on its own)\n"
    } else {
        "(indirectly -- the change came from its dependencies)\n"
    });
    out.push('\n');

    if e.root_causes.is_empty() {
        out.push_str(
            "No root cause could be attributed. This target's hash changed but none of its deps in \
             the dep-edges file are impacted -- most often because the hash JSON was filtered \
             with --targetType while the dep-edges file was not.\n\
             See https://github.com/Tinder/bazel-diff/issues/268\n",
        );
        return out;
    }

    if e.truncated {
        out.push_str(&format!(
            "Root causes: {} (showing the {} nearest)\n",
            e.total_root_causes,
            e.root_causes.len()
        ));
    } else {
        out.push_str(&format!("Root causes: {}\n", e.total_root_causes));
    }
    out.push('\n');

    let last = e.root_causes.len() - 1;
    for (index, cause) in e.root_causes.iter().enumerate() {
        let reason = match cause.kind {
            RootCauseKind::NewTarget => "new target in the final revision",
            RootCauseKind::SelfChanged => self_changed_reason(&cause.target_type),
        };
        out.push_str(&format!(
            "  {}. {}{}\n",
            index + 1,
            cause.label,
            type_suffix(&cause.target_type)
        ));
        out.push_str(&format!("     {reason}\n"));
        out.push_str(&format!(
            "     {}\n",
            hops(cause.target_distance, cause.package_hops)
        ));
        out.push_str(&format!("     {}\n", cause.path.join(" -> ")));
        if index != last {
            out.push('\n');
        }
    }

    if e.truncated {
        out.push('\n');
        out.push_str(&format!(
            "  ... and {} more. Pass --maxRootCauses=0 to list every root cause.\n",
            e.total_root_causes - e.root_causes.len()
        ));
    }
    out
}

fn self_changed_reason(target_type: &str) -> &'static str {
    match target_type {
        "SourceFile" => "source file content changed",
        "GeneratedFile" => "generated file changed",
        "Rule" => "the rule's own definition or attributes changed",
        _ => "the target's own hash changed",
    }
}

fn hops(target_distance: usize, package_hops: usize) -> String {
    if target_distance == 0 {
        return "0 hops -- this is the queried target itself".to_owned();
    }
    let hop_word = if target_distance == 1 { "hop" } else { "hops" };
    let package_word = if package_hops == 1 {
        "package boundary"
    } else {
        "package boundaries"
    };
    format!("{target_distance} {hop_word} ({package_hops} {package_word} crossed)")
}

fn node_ids(e: &ImpactExplanation) -> HashMap<&str, String> {
    e.nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.label.as_str(), format!("n{index}")))
        .collect()
}

fn render_dot(e: &ImpactExplanation) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "// bazel-diff explain -- why {} was impacted\n",
        dot_escape(&e.target)
    ));
    out.push_str(
        "// Edges point in impact-propagation direction: root cause -> ... -> queried target.\n",
    );
    out.push_str("digraph bazel_diff_impact {\n");
    out.push_str("  rankdir=TB;\n");
    out.push_str(&format!("  bgcolor=\"{SURFACE}\";\n"));
    out.push_str(&format!(
        "  node [shape=box, style=\"rounded,filled\", fillcolor=\"{SURFACE}\", \
         fontname=\"Helvetica\", fontsize=10, fontcolor=\"{INK}\", \
         color=\"{HUE_INTERMEDIATE}\", penwidth=1];\n"
    ));
    out.push_str(&format!(
        "  edge [color=\"{INK_MUTED}\", penwidth=1, arrowsize=0.7];\n"
    ));
    out.push('\n');

    let ids = node_ids(e);
    for node in &e.nodes {
        out.push_str(&format!(
            "  {} [label=\"{}\"",
            ids[node.label.as_str()],
            dot_escape(&node_label(node))
        ));
        if node.is_queried_target {
            out.push_str(&format!(", color=\"{HUE_QUERIED}\", penwidth=2"));
        } else if node.is_root_cause {
            out.push_str(&format!(
                ", color=\"{HUE_ROOT_CAUSE}\", penwidth=2, peripheries=2"
            ));
        }
        out.push_str("];\n");
    }

    if !e.edges.is_empty() {
        out.push('\n');
    }
    for edge in &e.edges {
        if let (Some(from), Some(to)) = (ids.get(edge.from.as_str()), ids.get(edge.to.as_str())) {
            out.push_str(&format!("  {from} -> {to};\n"));
        }
    }

    out.push('\n');
    out.push_str("  subgraph cluster_legend {\n");
    out.push_str(&format!(
        "    label=\"Legend\"; fontname=\"Helvetica\"; fontsize=9; fontcolor=\"{INK_MUTED}\";\n"
    ));
    out.push_str(&format!(
        "    color=\"{INK_MUTED}\"; penwidth=1; style=dashed;\n"
    ));
    out.push_str(&format!(
        "    lg_root [label=\"root cause\", color=\"{HUE_ROOT_CAUSE}\", penwidth=2, peripheries=2];\n"
    ));
    out.push_str("    lg_mid [label=\"carries the change\"];\n");
    out.push_str(&format!(
        "    lg_target [label=\"queried target\", color=\"{HUE_QUERIED}\", penwidth=2];\n"
    ));
    out.push_str("    lg_root -> lg_mid -> lg_target [style=invis];\n");
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

fn render_mermaid(e: &ImpactExplanation) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "%% bazel-diff explain -- why {} was impacted\n",
        e.target
    ));
    out.push_str(
        "%% Edges point in impact-propagation direction: root cause -> ... -> queried target.\n",
    );
    out.push_str("flowchart TD\n");

    let ids = node_ids(e);
    for node in &e.nodes {
        let id = &ids[node.label.as_str()];
        let text = mermaid_escape(&node_label(node));
        // Distinct bracket shapes so the three roles stay legible with no color at all.
        let shaped = if node.is_queried_target {
            format!("{id}([\"{text}\"])")
        } else if node.is_root_cause {
            format!("{id}[[\"{text}\"]]")
        } else {
            format!("{id}[\"{text}\"]")
        };
        out.push_str(&format!("  {shaped}\n"));
    }

    for edge in &e.edges {
        if let (Some(from), Some(to)) = (ids.get(edge.from.as_str()), ids.get(edge.to.as_str())) {
            out.push_str(&format!("  {from} --> {to}\n"));
        }
    }

    out.push_str(&format!(
        "  classDef rootCause fill:{SURFACE},stroke:{HUE_ROOT_CAUSE},stroke-width:2px,color:{INK}\n"
    ));
    out.push_str(&format!(
        "  classDef intermediate fill:{SURFACE},stroke:{HUE_INTERMEDIATE},stroke-width:1px,color:{INK}\n"
    ));
    out.push_str(&format!(
        "  classDef queried fill:{SURFACE},stroke:{HUE_QUERIED},stroke-width:2px,color:{INK}\n"
    ));

    let mut assign = |class_name: &str, predicate: fn(&ExplainNode) -> bool| {
        let matching = e
            .nodes
            .iter()
            .filter(|node| predicate(node))
            .map(|node| ids[node.label.as_str()].as_str())
            .collect::<Vec<_>>();
        if !matching.is_empty() {
            out.push_str(&format!("  class {} {class_name}\n", matching.join(",")));
        }
    };
    assign("queried", |node| node.is_queried_target);
    assign("rootCause", |node| {
        node.is_root_cause && !node.is_queried_target
    });
    assign("intermediate", |node| {
        !node.is_root_cause && !node.is_queried_target
    });
    out
}

/// The node's caption. Root causes and the queried target are direct-labeled
/// with their role on a second line, so the graph is fully readable in
/// monochrome or by a viewer who cannot separate the two hues.
fn node_label(node: &ExplainNode) -> String {
    let role = match (node.is_queried_target, node.is_root_cause) {
        (true, true) => Some("queried target - also a root cause"),
        (true, false) => Some("queried target"),
        (false, true) => Some("root cause"),
        (false, false) => None,
    };
    match role {
        Some(role) => format!("{}\n({role})", node.label),
        None => node.label.clone(),
    }
}

fn dot_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn mermaid_escape(value: &str) -> String {
    value.replace('"', "#quot;").replace('\n', "<br/>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(kind: &str, hash: &str, direct: &str) -> TargetHash {
        TargetHash {
            kind: kind.to_owned(),
            hash: hash.to_owned(),
            direct_hash: direct.to_owned(),
            deps: Vec::new(),
        }
    }

    fn hashes(entries: &[(&str, &str, &str)]) -> BTreeMap<String, TargetHash> {
        entries
            .iter()
            .map(|(label, hash_value, direct)| {
                let kind = if label.ends_with(".py") {
                    "SourceFile"
                } else {
                    "Rule"
                };
                ((*label).to_owned(), hash(kind, hash_value, direct))
            })
            .collect()
    }

    // //common:util.py -> //common:gen -> //common:lib -> //service/a:app
    fn dep_edges() -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([
            (
                "//service/a:app".to_owned(),
                vec!["//common:lib".to_owned(), "//service/a:main.py".to_owned()],
            ),
            ("//common:lib".to_owned(), vec!["//common:gen".to_owned()]),
            (
                "//common:gen".to_owned(),
                vec!["//common:util.py".to_owned()],
            ),
        ])
    }

    fn unchanged_base() -> BTreeMap<String, TargetHash> {
        hashes(&[
            ("//service/a:app", "app", "app-direct"),
            ("//service/a:main.py", "main", "main"),
            ("//common:lib", "lib", "lib-direct"),
            ("//common:gen", "gen", "gen-direct"),
            ("//common:util.py", "util", "util"),
        ])
    }

    fn upstream_change() -> BTreeMap<String, TargetHash> {
        // util.py's content changed; only its directHash moves, everything
        // downstream sees a transitive-hash change only.
        hashes(&[
            ("//service/a:app", "app2", "app-direct"),
            ("//service/a:main.py", "main", "main"),
            ("//common:lib", "lib2", "lib-direct"),
            ("//common:gen", "gen2", "gen-direct"),
            ("//common:util.py", "util2", "util2"),
        ])
    }

    fn run(
        from: &BTreeMap<String, TargetHash>,
        to: &BTreeMap<String, TargetHash>,
        edges: &BTreeMap<String, Vec<String>>,
        target: &str,
        limits: ExplainLimits,
    ) -> ImpactExplanation {
        explain(from, to, edges, target, limits)
            .unwrap()
            .explanation
    }

    #[test]
    fn attributes_an_indirect_impact_to_the_upstream_source_change() {
        let result = run(
            &unchanged_base(),
            &upstream_change(),
            &dep_edges(),
            "//service/a:app",
            ExplainLimits::default(),
        );

        assert!(result.impacted);
        assert!(!result.directly_changed);
        assert_eq!(result.total_root_causes, 1);
        let cause = &result.root_causes[0];
        assert_eq!(cause.label, "//common:util.py");
        assert_eq!(cause.kind, RootCauseKind::SelfChanged);
        assert_eq!(cause.target_distance, 3);
        // //common -> //common -> //service/a: exactly one boundary crossed.
        assert_eq!(cause.package_hops, 1);
        assert_eq!(
            cause.path,
            [
                "//common:util.py",
                "//common:gen",
                "//common:lib",
                "//service/a:app"
            ]
        );
    }

    #[test]
    fn reports_every_reason_when_a_target_changed_itself_and_upstream() {
        // The exact ambiguity issue #479 asks about: service A changed AND the
        // common module changed.
        let to = hashes(&[
            ("//service/a:app", "app2", "app-direct2"),
            ("//service/a:main.py", "main2", "main2"),
            ("//common:lib", "lib2", "lib-direct"),
            ("//common:gen", "gen2", "gen-direct"),
            ("//common:util.py", "util2", "util2"),
        ]);

        let result = run(
            &unchanged_base(),
            &to,
            &dep_edges(),
            "//service/a:app",
            ExplainLimits::default(),
        );

        assert!(result.directly_changed);
        let labels = result
            .root_causes
            .iter()
            .map(|cause| cause.label.as_str())
            .collect::<Vec<_>>();
        // Nearest-first ordering: the target itself, then its own source, then the upstream module.
        assert_eq!(
            labels,
            ["//service/a:app", "//service/a:main.py", "//common:util.py"]
        );
        let distances = result
            .root_causes
            .iter()
            .map(|cause| cause.target_distance)
            .collect::<Vec<_>>();
        assert_eq!(distances, [0, 1, 3]);
    }

    #[test]
    fn walks_through_a_directly_changed_dep_to_find_further_root_causes() {
        // //common:gen changed its own definition AND consumes a changed
        // source. Both are genuine reasons the app moved, so the walk must not
        // stop at the first DIRECT label it reaches.
        let to = hashes(&[
            ("//service/a:app", "app2", "app-direct"),
            ("//service/a:main.py", "main", "main"),
            ("//common:lib", "lib2", "lib-direct"),
            ("//common:gen", "gen2", "gen-direct2"),
            ("//common:util.py", "util2", "util2"),
        ]);

        let result = run(
            &unchanged_base(),
            &to,
            &dep_edges(),
            "//service/a:app",
            ExplainLimits::default(),
        );

        let labels = result
            .root_causes
            .iter()
            .map(|cause| cause.label.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(labels, BTreeSet::from(["//common:gen", "//common:util.py"]));
    }

    #[test]
    fn classifies_a_target_absent_from_the_starting_revision_as_new() {
        let mut from = unchanged_base();
        from.remove("//common:util.py");
        let to = hashes(&[
            ("//service/a:app", "app2", "app-direct"),
            ("//service/a:main.py", "main", "main"),
            ("//common:lib", "lib2", "lib-direct"),
            ("//common:gen", "gen2", "gen-direct"),
            ("//common:util.py", "util", "util"),
        ]);

        let result = run(
            &from,
            &to,
            &dep_edges(),
            "//service/a:app",
            ExplainLimits::default(),
        );

        assert_eq!(result.root_causes.len(), 1);
        assert_eq!(result.root_causes[0].kind, RootCauseKind::NewTarget);
    }

    #[test]
    fn reports_an_unimpacted_target_as_such_and_not_removed() {
        let base = unchanged_base();

        let result = run(
            &base,
            &base,
            &dep_edges(),
            "//service/a:app",
            ExplainLimits::default(),
        );

        assert!(!result.impacted);
        assert!(!result.removed);
        assert!(result.root_causes.is_empty());
        assert!(result.nodes.is_empty());
        assert!(result.edges.is_empty());
        assert_eq!(result.target_type, "Rule");
    }

    #[test]
    fn distinguishes_a_deleted_target_from_an_unchanged_one() {
        // bazel-diff never reports a deleted target as impacted, but "deleted"
        // and "hash unchanged" are different facts and must not collapse.
        let from = unchanged_base();
        let mut to = from.clone();
        to.remove("//service/a:app");

        let result = run(
            &from,
            &to,
            &dep_edges(),
            "//service/a:app",
            ExplainLimits::default(),
        );

        assert!(!result.impacted);
        assert!(result.removed);
    }

    #[test]
    fn rejects_a_label_present_in_neither_revision() {
        let base = unchanged_base();

        let error = explain(
            &base,
            &base,
            &dep_edges(),
            "//typo:nope",
            ExplainLimits::default(),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "//typo:nope is not present in either hash file. Check the label spelling, and note \
             that hashes generated with --targetType only contain the types you asked for."
        );
    }

    #[test]
    fn truncates_to_max_root_causes_but_still_reports_the_total() {
        // Five independently-changed sources all feeding one app.
        let sources = (1..=5)
            .map(|index| format!("//common:s{index}.py"))
            .collect::<Vec<_>>();
        let edges = BTreeMap::from([("//app:app".to_owned(), sources.clone())]);
        let mut from = sources
            .iter()
            .map(|label| (label.clone(), hash("SourceFile", "v1", "v1")))
            .collect::<BTreeMap<_, _>>();
        from.insert("//app:app".to_owned(), hash("Rule", "app", "app"));
        let mut to = sources
            .iter()
            .map(|label| (label.clone(), hash("SourceFile", "v2", "v2")))
            .collect::<BTreeMap<_, _>>();
        to.insert("//app:app".to_owned(), hash("Rule", "app2", "app"));

        let outcome = explain(
            &from,
            &to,
            &edges,
            "//app:app",
            ExplainLimits {
                max_depth: None,
                max_roots: 2,
            },
        )
        .unwrap();
        let result = outcome.explanation;

        assert_eq!(result.root_causes.len(), 2);
        assert_eq!(result.total_root_causes, 5);
        assert!(result.truncated);
        assert_eq!(outcome.notes.len(), 1);
        assert!(outcome.notes[0].contains("Reporting 2 of 5 root causes"));
        // Nearest-first, then lexicographic -- so the cap is deterministic.
        let labels = result
            .root_causes
            .iter()
            .map(|cause| cause.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["//common:s1.py", "//common:s2.py"]);
        // The subgraph spans only the reported causes.
        let nodes = result
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            nodes,
            BTreeSet::from(["//app:app", "//common:s1.py", "//common:s2.py"])
        );
    }

    #[test]
    fn max_depth_bounds_the_search_and_warns() {
        // util.py sits 3 hops up; a 2-hop budget cannot reach it.
        let outcome = explain(
            &unchanged_base(),
            &upstream_change(),
            &dep_edges(),
            "//service/a:app",
            ExplainLimits {
                max_depth: Some(2),
                max_roots: 0,
            },
        )
        .unwrap();

        assert!(outcome.explanation.impacted);
        assert!(outcome.explanation.root_causes.is_empty());
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("--maxDepth=2"));
    }

    #[test]
    fn subgraph_includes_every_edge_between_included_nodes_not_just_path_edges() {
        // Diamond: two independent paths from the changed source down to the
        // app. The BFS tree keeps one parent per node, but every edge between
        // two included nodes is rendered, oriented root -> target.
        let edges = BTreeMap::from([
            (
                "//app:app".to_owned(),
                vec!["//mid:a".to_owned(), "//mid:b".to_owned()],
            ),
            ("//mid:a".to_owned(), vec!["//src:x.py".to_owned()]),
            ("//mid:b".to_owned(), vec!["//src:x.py".to_owned()]),
        ]);
        let from = BTreeMap::from([
            ("//app:app".to_owned(), hash("Rule", "app", "app")),
            ("//mid:a".to_owned(), hash("Rule", "a", "a")),
            ("//mid:b".to_owned(), hash("Rule", "b", "b")),
            ("//src:x.py".to_owned(), hash("SourceFile", "x", "x")),
        ]);
        let to = BTreeMap::from([
            ("//app:app".to_owned(), hash("Rule", "app2", "app")),
            ("//mid:a".to_owned(), hash("Rule", "a2", "a")),
            ("//mid:b".to_owned(), hash("Rule", "b2", "b")),
            ("//src:x.py".to_owned(), hash("SourceFile", "x2", "x2")),
        ]);

        let result = run(&from, &to, &edges, "//app:app", ExplainLimits::default());

        assert_eq!(result.root_causes.len(), 1);
        assert_eq!(result.root_causes[0].label, "//src:x.py");
        let labels = result
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"//app:app"));
        assert!(labels.contains(&"//src:x.py"));
        assert!(!result.edges.is_empty());
        for edge in &result.edges {
            assert!(edges[&edge.to].contains(&edge.from));
        }
        // Nodes are ordered most-distant first, so DOT/Mermaid read top-down.
        assert_eq!(result.nodes[0].label, "//src:x.py");
        assert!(result.nodes.last().unwrap().is_queried_target);
    }

    #[test]
    fn reports_no_root_cause_when_no_dep_is_impacted() {
        // The #268 shape: the hash JSON was filtered by --targetType so the
        // changed source is absent, leaving an impacted Rule whose deps are all
        // unimpacted. Must degrade, not crash.
        let from = BTreeMap::from([("//app:app".to_owned(), hash("Rule", "app", "app"))]);
        let to = BTreeMap::from([("//app:app".to_owned(), hash("Rule", "app2", "app"))]);
        let edges = BTreeMap::from([("//app:app".to_owned(), vec!["//src:x.py".to_owned()])]);

        let result = run(&from, &to, &edges, "//app:app", ExplainLimits::default());

        assert!(result.impacted);
        assert!(!result.directly_changed);
        assert!(result.root_causes.is_empty());
        assert_eq!(result.nodes.len(), 1);
    }

    #[test]
    fn handles_an_empty_target_type_when_hashes_were_generated_without_it() {
        let from = BTreeMap::from([("//app:app".to_owned(), hash("", "app", "app"))]);
        let to = BTreeMap::from([("//app:app".to_owned(), hash("", "app2", "app2"))]);

        let result = run(
            &from,
            &to,
            &BTreeMap::new(),
            "//app:app",
            ExplainLimits::default(),
        );

        assert_eq!(result.target_type, "");
        assert_eq!(result.root_causes[0].target_type, "");
        assert_eq!(result.root_causes[0].kind, RootCauseKind::SelfChanged);
        assert_eq!(result.root_causes[0].target_distance, 0);
        assert_eq!(result.root_causes[0].package_hops, 0);
    }

    // ----- rendering -----

    fn explanation() -> ImpactExplanation {
        ImpactExplanation {
            target: "//service/a:app".to_owned(),
            target_type: "Rule".to_owned(),
            impacted: true,
            removed: false,
            directly_changed: false,
            root_causes: vec![RootCause {
                label: "//common:util.py".to_owned(),
                target_type: "SourceFile".to_owned(),
                kind: RootCauseKind::SelfChanged,
                target_distance: 2,
                package_hops: 1,
                path: vec![
                    "//common:util.py".to_owned(),
                    "//common:lib".to_owned(),
                    "//service/a:app".to_owned(),
                ],
            }],
            total_root_causes: 1,
            truncated: false,
            nodes: vec![
                node("//common:util.py", "SourceFile", "direct", 2, true, false),
                node("//common:lib", "Rule", "indirect", 1, false, false),
                node("//service/a:app", "Rule", "indirect", 0, false, true),
            ],
            edges: vec![
                ExplainEdge {
                    from: "//common:lib".to_owned(),
                    to: "//service/a:app".to_owned(),
                },
                ExplainEdge {
                    from: "//common:util.py".to_owned(),
                    to: "//common:lib".to_owned(),
                },
            ],
        }
    }

    fn node(
        label: &str,
        target_type: &str,
        impact_type: &str,
        distance: usize,
        root: bool,
        queried: bool,
    ) -> ExplainNode {
        ExplainNode {
            label: label.to_owned(),
            target_type: target_type.to_owned(),
            impact_type: impact_type.to_owned(),
            target_distance: distance,
            is_root_cause: root,
            is_queried_target: queried,
        }
    }

    #[test]
    fn text_names_the_root_cause_the_reason_and_the_path() {
        let out = render(&explanation(), ExplainFormat::Text);

        assert!(out.contains("IMPACTED (indirectly"));
        assert!(out.contains("Root causes: 1"));
        assert!(out.contains("//common:util.py  [SourceFile]"));
        assert!(out.contains("source file content changed"));
        assert!(out.contains("2 hops (1 package boundary crossed)"));
        assert!(out.contains("//common:util.py -> //common:lib -> //service/a:app"));
    }

    #[test]
    fn text_distinguishes_unimpacted_removed_and_direct_changes() {
        let mut unimpacted = explanation();
        unimpacted.impacted = false;
        unimpacted.root_causes.clear();
        unimpacted.total_root_causes = 0;
        let out = render(&unimpacted, ExplainFormat::Text);
        assert!(out.contains("NOT IMPACTED -- its hash is identical"));
        assert!(!out.contains("Root causes"));

        let mut removed = unimpacted.clone();
        removed.removed = true;
        let out = render(&removed, ExplainFormat::Text);
        assert!(out.contains("exists in the starting revision but not the final one"));
        assert!(!out.contains("hash is identical"));

        let mut direct = explanation();
        direct.directly_changed = true;
        let out = render(&direct, ExplainFormat::Text);
        assert!(out.contains("IMPACTED (directly -- this target changed on its own)"));
    }

    #[test]
    fn text_surfaces_the_truncated_count_rather_than_capping_silently() {
        let mut truncated = explanation();
        truncated.total_root_causes = 9;
        truncated.truncated = true;

        let out = render(&truncated, ExplainFormat::Text);

        assert!(out.contains("Root causes: 9 (showing the 1 nearest)"));
        assert!(out.contains("... and 8 more"));
        assert!(out.contains("--maxRootCauses=0"));
    }

    #[test]
    fn text_points_at_issue_268_when_nothing_could_be_attributed() {
        let mut unattributed = explanation();
        unattributed.root_causes.clear();
        unattributed.total_root_causes = 0;

        let out = render(&unattributed, ExplainFormat::Text);

        assert!(out.contains("No root cause could be attributed"));
        assert!(out.contains("--targetType"));
        assert!(out.contains("issues/268"));
    }

    #[test]
    fn text_pluralises_hops_and_describes_new_and_zero_hop_causes() {
        let mut single = explanation();
        single.root_causes = vec![RootCause {
            label: "//common:lib".to_owned(),
            target_type: "Rule".to_owned(),
            kind: RootCauseKind::NewTarget,
            target_distance: 1,
            package_hops: 0,
            path: vec!["//common:lib".to_owned(), "//service/a:app".to_owned()],
        }];
        let out = render(&single, ExplainFormat::Text);
        assert!(out.contains("1 hop (0 package boundaries crossed)"));
        assert!(out.contains("new target in the final revision"));

        let mut zero = explanation();
        zero.directly_changed = true;
        zero.root_causes = vec![RootCause {
            label: "//service/a:app".to_owned(),
            target_type: "Rule".to_owned(),
            kind: RootCauseKind::SelfChanged,
            target_distance: 0,
            package_hops: 0,
            path: vec!["//service/a:app".to_owned()],
        }];
        let out = render(&zero, ExplainFormat::Text);
        assert!(out.contains("0 hops -- this is the queried target itself"));
        assert!(out.contains("the rule's own definition or attributes changed"));

        assert_eq!(
            self_changed_reason("GeneratedFile"),
            "generated file changed"
        );
        assert_eq!(self_changed_reason(""), "the target's own hash changed");
    }

    #[test]
    fn dot_emits_nodes_edges_a_legend_and_stroke_colored_identity() {
        let out = render(&explanation(), ExplainFormat::Dot);

        assert!(out.contains("digraph bazel_diff_impact {"));
        assert!(out.contains("rankdir=TB;"));
        // Edges run root cause -> queried target: n0 is the most-distant node, n2 the target.
        assert!(out.contains("n0 [label=\"//common:util.py\\n(root cause)\""));
        assert!(out.contains("n2 [label=\"//service/a:app\\n(queried target)\""));
        assert!(out.contains("n0 -> n1;"));
        assert!(out.contains("n1 -> n2;"));
        assert!(out.contains("subgraph cluster_legend {"));
        // Identity lives on the stroke; every node shares one surface fill.
        assert!(out.contains("color=\"#eb6834\", penwidth=2, peripheries=2"));
        assert!(out.contains("color=\"#2a78d6\", penwidth=2"));
        assert!(out.contains("fillcolor=\"#fcfcfb\""));
        assert!(out.contains("bgcolor=\"#fcfcfb\";"));
    }

    #[test]
    fn dot_escapes_quotes_and_backslashes_in_labels() {
        let mut awkward = explanation();
        awkward.nodes = vec![node("//weird:a\"b\\c", "Rule", "direct", 0, false, false)];
        awkward.edges.clear();

        let out = render(&awkward, ExplainFormat::Dot);

        assert!(out.contains("n0 [label=\"//weird:a\\\"b\\\\c\"];"));
    }

    #[test]
    fn mermaid_shapes_each_role_distinctly_so_color_is_never_the_only_encoding() {
        let out = render(&explanation(), ExplainFormat::Mermaid);

        assert!(out.contains("flowchart TD"));
        assert!(out.contains("n0[[\"//common:util.py<br/>(root cause)\"]]"));
        assert!(out.contains("n1[\"//common:lib\"]"));
        assert!(out.contains("n2([\"//service/a:app<br/>(queried target)\"])"));
        assert!(out.contains("n0 --> n1"));
        assert!(out.contains("class n2 queried"));
        assert!(out.contains("class n0 rootCause"));
        assert!(out.contains("class n1 intermediate"));
    }

    #[test]
    fn mermaid_escapes_quotes_and_omits_class_lines_for_empty_roles() {
        let mut quoted = explanation();
        quoted.nodes = vec![node("//weird:a\"b", "Rule", "direct", 0, false, false)];
        quoted.edges.clear();
        let out = render(&quoted, ExplainFormat::Mermaid);
        assert!(out.contains("n0[\"//weird:a#quot;b\"]"));

        let mut both = explanation();
        both.nodes = vec![node("//app:app", "Rule", "direct", 0, true, true)];
        both.edges.clear();
        let out = render(&both, ExplainFormat::Mermaid);
        assert!(out.contains("class n0 queried"));
        assert!(!out.contains("class  intermediate"));
        assert!(!out.contains("class n0 rootCause"));
        // A node that is both the target and a root cause says so directly.
        assert!(out.contains("(queried target - also a root cause)"));
    }

    #[test]
    fn json_round_trips_the_whole_explanation_with_the_documented_field_names() {
        let out = render(&explanation(), ExplainFormat::Json);

        assert!(out.contains("\"rootCauses\""));
        assert!(out.contains("\"totalRootCauses\""));
        assert!(out.contains("\"packageHops\""));
        assert!(out.contains("\"kind\": \"SELF_CHANGED\""));
        assert!(out.contains("\"isQueriedTarget\""));
        assert!(out.ends_with('\n'));
        let parsed: ImpactExplanation = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, explanation());
    }
}
