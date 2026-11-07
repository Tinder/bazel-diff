//! Positive ordinary evidence about toolchain users, separate from hash distances.
use crate::model::{HashFileData, TargetHash};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Declaration {
    pub types: BTreeSet<String>,
    pub implementations: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedToolchains {
    pub version: u32,
    pub declarations: BTreeMap<String, Declaration>,
    // Canonical identity -> actual key in this endpoint's hash file.
    pub hash_labels: BTreeMap<String, String>,
    pub readers: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unresolved: BTreeSet<String>,
}

impl ObservedToolchains {
    fn hash<'a>(&self, file: &'a HashFileData, name: &str) -> Option<&'a TargetHash> {
        self.hash_labels
            .get(name)
            .and_then(|label| file.hashes.get(label))
            .or_else(|| file.hashes.get(name))
    }
}

#[derive(Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateReport {
    /// The positive comparison is bounded by observed ordinary declarations.
    pub possible_types: BTreeSet<String>,
    pub unresolved: BTreeSet<String>,
    pub possible_readers: BTreeSet<String>,
    /// Loading-phase observations cannot certify registration order, command-line
    /// providers, or the absence of hidden edges in another producer.
    pub complete: bool,
}

pub fn compare(from: &HashFileData, to: &HashFileData) -> CandidateReport {
    let mut result = CandidateReport::default();
    let before = from.observed_toolchains.as_ref().filter(|v| v.version == 1);
    let after = to.observed_toolchains.as_ref().filter(|v| v.version == 1);
    if before.is_none() {
        result
            .unresolved
            .insert("base declaration observation unavailable".into());
    }
    if after.is_none() {
        result
            .unresolved
            .insert("head declaration observation unavailable".into());
    }
    let names: BTreeSet<_> = before
        .into_iter()
        .chain(after)
        .flat_map(|v| v.declarations.keys())
        .collect();
    for index in before.into_iter().chain(after) {
        result.unresolved.extend(index.unresolved.iter().cloned());
    }
    let module_changed = from.module_graph_json != to.module_graph_json;
    if module_changed {
        result
            .unresolved
            .insert("observed module graph changed; registration order is unavailable".into());
    }
    for name in names {
        let old = before.and_then(|v| v.declarations.get(name));
        let new = after.and_then(|v| v.declarations.get(name));
        let mut possible = module_changed || old != new || before.is_none() || after.is_none();
        let hash_pair = |label: &str| {
            (
                before.and_then(|v| v.hash(from, label)),
                after.and_then(|v| v.hash(to, label)),
            )
        };
        let mut operands: BTreeSet<&String> = old
            .into_iter()
            .chain(new)
            .flat_map(|d| d.implementations.iter().chain(&d.types))
            .collect();
        if old.is_some() && new.is_some() {
            operands.insert(name);
        }
        for operand in operands {
            possible |= result.unresolved.contains(operand);
            match hash_pair(operand) {
                (Some(a), Some(b)) => possible |= !a.same_as(b),
                _ => {
                    possible = true;
                    result.unresolved.insert(operand.clone());
                }
            }
        }
        let types = old
            .into_iter()
            .chain(new)
            .flat_map(|d| d.types.iter().cloned());
        if possible {
            result.possible_types.extend(types);
        }
    }
    result
}

pub fn add_returned_readers(
    report: &mut CandidateReport,
    head: &HashFileData,
    edges: &BTreeMap<String, Vec<String>>,
) {
    let Some(index) = head
        .observed_toolchains
        .as_ref()
        .filter(|v| v.version == 1)
        .and_then(|v| v.readers.as_ref())
    else {
        report
            .unresolved
            .insert("head reader observation unavailable".into());
        return;
    };
    if report.possible_types.is_empty() {
        return;
    }
    report.unresolved.extend(
        report
            .possible_types
            .iter()
            .filter(|ty| !index.contains_key(*ty))
            .cloned(),
    );
    let mut queue: VecDeque<_> = report
        .possible_types
        .iter()
        .filter_map(|ty| index.get(ty))
        .flatten()
        .map(String::as_str)
        .collect();
    if queue.is_empty() {
        return;
    }
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (parent, inputs) in edges {
        for child in inputs {
            parents.entry(child).or_default().push(parent);
        }
    }
    let mut visited = HashSet::new();
    while let Some(reader) = queue.pop_front() {
        if !visited.insert(reader) {
            continue;
        }
        if head.hashes.contains_key(reader) {
            report.possible_readers.insert(reader.to_owned());
        }
        queue.extend(parents.get(reader).into_iter().flatten().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_uses_base_binding_and_requires_observed_head_readers() {
        let payload = br#"{"hashes":{"//:body":"Rule#1~1","//:reader":"Rule#9~9","//:user":"Rule#7~7","//:type":"Rule#5~5"},"metadata":{"observedToolchains":{"version":1,"declarations":{"//:registration":{"types":["//:type"],"implementations":["//:body"]}},"hashLabels":{"//:body":"//:body","//:type":"//:type"},"readers":null}}}"#;
        let before = HashFileData::from_slice(payload).unwrap();
        let mut after = before.clone();
        let index = after.observed_toolchains.as_mut().unwrap();
        index.declarations.clear();
        index.hash_labels.clear();
        index.readers = Some(BTreeMap::from([(
            "//:type".into(),
            vec!["//:reader".into()],
        )]));
        let after =
            HashFileData::from_slice(&serde_json::to_vec(&after.serialized(true, false)).unwrap())
                .unwrap();
        let edges = BTreeMap::from([
            ("//:user".into(), vec!["//:generated".into()]),
            ("//:generated".into(), vec!["//:reader".into()]),
        ]);
        let mut result = compare(&before, &after);
        add_returned_readers(&mut result, &after, &edges);
        assert_eq!(result.possible_types, BTreeSet::from(["//:type".into()]));
        assert_eq!(
            result.possible_readers,
            BTreeSet::from(["//:reader".into(), "//:user".into()])
        );
        assert!(result.unresolved.is_empty());
        assert!(!result.complete);
        let mut unsupported = after;
        unsupported.observed_toolchains.as_mut().unwrap().version = 2;
        let mut result = compare(&before, &unsupported);
        add_returned_readers(&mut result, &unsupported, &edges);
        assert!(result.possible_readers.is_empty());
        assert!(!result.unresolved.is_empty());
    }
}
