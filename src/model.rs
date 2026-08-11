use anyhow::{anyhow, bail, Context, Result};
use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetHash {
    pub kind: String,
    pub hash: String,
    pub direct_hash: String,
    pub deps: Vec<String>,
}

impl TargetHash {
    pub fn parse(value: &str) -> Result<Self> {
        let (kind, hashes) = value
            .split_once('#')
            .map_or(("", value), |(kind, hashes)| (kind, hashes));
        let (hash, direct_hash) = hashes
            .split_once('~')
            .ok_or_else(|| anyhow!("Invalid targetHash format: {value}"))?;
        if direct_hash.contains('~') || kind.contains('#') {
            bail!("Invalid targetHash format: {value}");
        }
        Ok(Self {
            kind: kind.to_owned(),
            hash: hash.to_owned(),
            direct_hash: direct_hash.to_owned(),
            deps: Vec::new(),
        })
    }

    pub fn json_value(&self, include_target_type: bool) -> String {
        if include_target_type {
            format!("{}#{}~{}", self.kind, self.hash, self.direct_hash)
        } else {
            format!("{}~{}", self.hash, self.direct_hash)
        }
    }

    fn identity(&self) -> String {
        format!("{}#{}~{}", self.kind, self.hash, self.direct_hash)
    }
}

struct SerializedTargetHash<'a> {
    hash: &'a TargetHash,
    include_target_type: bool,
}

impl Serialize for SerializedTargetHash<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.include_target_type {
            serializer.collect_str(&format_args!(
                "{}#{}~{}",
                self.hash.kind, self.hash.hash, self.hash.direct_hash
            ))
        } else {
            serializer.collect_str(&format_args!(
                "{}~{}",
                self.hash.hash, self.hash.direct_hash
            ))
        }
    }
}

struct SerializedHashes<'a> {
    hashes: &'a BTreeMap<String, TargetHash>,
    include_target_type: bool,
}

impl Serialize for SerializedHashes<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.hashes.len()))?;
        for (label, hash) in self.hashes {
            map.serialize_entry(
                label,
                &SerializedTargetHash {
                    hash,
                    include_target_type: self.include_target_type,
                },
            )?;
        }
        map.end()
    }
}

struct SerializedMetadata<'a> {
    data: &'a HashFileData,
    include_deps: bool,
}

impl Serialize for SerializedMetadata<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let include_dep_edges = self.include_deps && !self.data.dep_edges.is_empty();
        let mut state = serializer.serialize_struct(
            "HashMetadata",
            usize::from(self.data.module_graph_json.is_some()) + usize::from(include_dep_edges),
        )?;
        if let Some(module_graph_json) = &self.data.module_graph_json {
            state.serialize_field("moduleGraphJson", module_graph_json)?;
        }
        if include_dep_edges {
            state.serialize_field("depEdges", &self.data.dep_edges)?;
        }
        state.end()
    }
}

pub struct SerializedHashFile<'a> {
    data: &'a HashFileData,
    include_target_type: bool,
    include_deps: bool,
}

impl Serialize for SerializedHashFile<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hashes = SerializedHashes {
            hashes: &self.data.hashes,
            include_target_type: self.include_target_type,
        };
        if self.data.module_graph_json.is_none()
            && (!self.include_deps || self.data.dep_edges.is_empty())
        {
            return hashes.serialize(serializer);
        }
        let mut state = serializer.serialize_struct("HashFileData", 2)?;
        state.serialize_field("hashes", &hashes)?;
        state.serialize_field(
            "metadata",
            &SerializedMetadata {
                data: self.data,
                include_deps: self.include_deps,
            },
        )?;
        state.end()
    }
}

#[derive(Clone, Debug, Default)]
pub struct HashFileData {
    pub hashes: BTreeMap<String, TargetHash>,
    pub module_graph_json: Option<String>,
    pub dep_edges: BTreeMap<String, Vec<String>>,
}

impl HashFileData {
    pub fn serialized(
        &self,
        include_target_type: bool,
        include_deps: bool,
    ) -> SerializedHashFile<'_> {
        SerializedHashFile {
            data: self,
            include_target_type,
            include_deps,
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read hash file {}", path.display()))?;
        Self::from_slice(&bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let root: Value = serde_json::from_slice(bytes).context("invalid hash JSON")?;
        let object = root
            .as_object()
            .ok_or_else(|| anyhow!("hash JSON must be an object"))?;
        let (hashes_value, metadata) = match (object.get("hashes"), object.get("metadata")) {
            (Some(hashes), Some(metadata)) => (hashes, metadata.as_object()),
            _ => (&root, None),
        };
        let raw_hashes = hashes_value
            .as_object()
            .ok_or_else(|| anyhow!("hashes must be a JSON object"))?;
        let mut hashes = BTreeMap::new();
        for (label, value) in raw_hashes {
            hashes.insert(
                label.clone(),
                TargetHash::parse(
                    value
                        .as_str()
                        .ok_or_else(|| anyhow!("hash for {label} must be a string"))?,
                )?,
            );
        }
        let module_graph_json = metadata
            .and_then(|value| value.get("moduleGraphJson"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let dep_edges = metadata
            .and_then(|value| value.get("depEdges"))
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .context("metadata.depEdges must map labels to label arrays")?
            .unwrap_or_default();
        Ok(Self {
            hashes,
            module_graph_json,
            dep_edges,
        })
    }

    pub fn to_value(&self, include_target_type: bool, include_deps: bool) -> Value {
        let hashes = self
            .hashes
            .iter()
            .map(|(label, hash)| {
                (
                    label.clone(),
                    Value::String(hash.json_value(include_target_type)),
                )
            })
            .collect::<Map<_, _>>();
        if self.module_graph_json.is_none() && (!include_deps || self.dep_edges.is_empty()) {
            return Value::Object(hashes);
        }
        let mut metadata = Map::new();
        if let Some(module_graph_json) = &self.module_graph_json {
            metadata.insert(
                "moduleGraphJson".to_owned(),
                Value::String(module_graph_json.clone()),
            );
        }
        if include_deps && !self.dep_edges.is_empty() {
            metadata.insert(
                "depEdges".to_owned(),
                serde_json::to_value(&self.dep_edges).expect("serializing string map cannot fail"),
            );
        }
        serde_json::json!({"hashes": hashes, "metadata": metadata})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImpactType {
    Direct,
    Indirect,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactedTargetWithDistance {
    pub label: String,
    pub target_distance: usize,
    pub package_distance: usize,
}

fn kind_rank(hash: Option<&TargetHash>) -> u8 {
    match hash
        .map(|hash| hash.kind.as_str())
        .filter(|kind| !kind.is_empty())
    {
        Some("SourceFile") => 0,
        Some("GeneratedFile") => 1,
        Some("Rule") => 2,
        Some(_) => 3,
        None => 4,
    }
}

fn accepts(
    label: &str,
    to: &BTreeMap<String, TargetHash>,
    target_types: Option<&HashSet<String>>,
) -> Result<bool> {
    let Some(target_types) = target_types else {
        return Ok(true);
    };
    let hash = to
        .get(label)
        .ok_or_else(|| anyhow!("missing final hash for impacted target {label}"))?;
    if hash.kind.is_empty() {
        bail!(
            "No target type info found, please re-generate the target hashes JSON with --includeTargetType!"
        );
    }
    Ok(target_types.contains(&hash.kind))
}

pub fn impacted_targets(
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
) -> Result<Vec<String>> {
    let impacted = to
        .iter()
        .filter_map(|(label, hash)| {
            (from.get(label).map(TargetHash::identity) != Some(hash.identity()))
                .then_some(label.clone())
        })
        .filter(|label| !exclude_external || !label.starts_with("//external:"))
        .filter_map(|label| match accepts(&label, to, target_types) {
            Ok(true) => Some(Ok(label)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    filter_and_sort_labels(impacted, from, to, target_types, exclude_external)
}

pub fn filter_and_sort_labels(
    labels: impl IntoIterator<Item = String>,
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
) -> Result<Vec<String>> {
    let mut impacted = labels
        .into_iter()
        .filter(|label| !exclude_external || !label.starts_with("//external:"))
        .filter_map(|label| match accepts(&label, to, target_types) {
            Ok(true) => Some(Ok(label)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    impacted.sort_by_key(|label| {
        (
            kind_rank(to.get(label).or_else(|| from.get(label))),
            label.clone(),
        )
    });
    impacted.dedup();
    Ok(impacted)
}

fn impacted_types(
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
) -> HashMap<String, ImpactType> {
    to.iter()
        .filter_map(|(label, final_hash)| match from.get(label) {
            None => Some((label.clone(), ImpactType::Direct)),
            Some(starting_hash) if starting_hash.identity() != final_hash.identity() => Some((
                label.clone(),
                if starting_hash.direct_hash != final_hash.direct_hash {
                    ImpactType::Direct
                } else {
                    ImpactType::Indirect
                },
            )),
            _ => None,
        })
        .collect()
}

fn calculate_distance(
    label: &str,
    dep_edges: &BTreeMap<String, Vec<String>>,
    impact_types: &HashMap<String, ImpactType>,
    memo: &mut HashMap<String, (usize, usize)>,
    visiting: &mut BTreeSet<String>,
) -> (usize, usize) {
    if let Some(distance) = memo.get(label) {
        return *distance;
    }
    if impact_types.get(label) == Some(&ImpactType::Direct) {
        memo.insert(label.to_owned(), (0, 0));
        return (0, 0);
    }
    if !visiting.insert(label.to_owned()) {
        return (0, 0);
    }
    let impacted_deps = dep_edges
        .get(label)
        .into_iter()
        .flatten()
        .filter(|dep| impact_types.contains_key(*dep))
        .cloned()
        .collect::<Vec<_>>();
    if impacted_deps.is_empty() {
        eprintln!(
            "[Warn] {label} was indirectly impacted, but has no impacted dependencies in the dep-edges file. Falling back to distance 0 -- see https://github.com/Tinder/bazel-diff/issues/268"
        );
        visiting.remove(label);
        memo.insert(label.to_owned(), (0, 0));
        return (0, 0);
    }
    let label_package = label.split(':').next().unwrap_or(label);
    let mut target_distance = usize::MAX;
    let mut package_distance = usize::MAX;
    for dep in impacted_deps {
        let (dep_target, dep_package) =
            calculate_distance(&dep, dep_edges, impact_types, memo, visiting);
        target_distance = target_distance.min(dep_target.saturating_add(1));
        package_distance = package_distance
            .min(dep_package + usize::from(dep.split(':').next().unwrap_or(&dep) != label_package));
    }
    visiting.remove(label);
    let result = (target_distance, package_distance);
    memo.insert(label.to_owned(), result);
    result
}

pub fn impacted_targets_with_distances(
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
    dep_edges: &BTreeMap<String, Vec<String>>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
) -> Result<Vec<ImpactedTargetWithDistance>> {
    let impact_types = impacted_types(from, to);
    let mut memo = HashMap::new();
    for label in impact_types.keys() {
        calculate_distance(
            label,
            dep_edges,
            &impact_types,
            &mut memo,
            &mut BTreeSet::new(),
        );
    }
    let mut impacted = memo
        .into_iter()
        .filter(|(label, _)| !exclude_external || !label.starts_with("//external:"))
        .filter_map(|(label, (target_distance, package_distance))| {
            match accepts(&label, to, target_types) {
                Ok(true) => Some(Ok(ImpactedTargetWithDistance {
                    label,
                    target_distance,
                    package_distance,
                })),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    impacted.sort_by_key(|target| {
        (
            kind_rank(to.get(&target.label).or_else(|| from.get(&target.label))),
            target.label.clone(),
        )
    });
    Ok(impacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn target(kind: &str, hash: &str, direct: &str) -> TargetHash {
        TargetHash {
            kind: kind.to_owned(),
            hash: hash.to_owned(),
            direct_hash: direct.to_owned(),
            deps: Vec::new(),
        }
    }

    #[test]
    fn parses_both_hash_formats() {
        assert_eq!(
            TargetHash::parse("Rule#a~b").unwrap(),
            target("Rule", "a", "b")
        );
        assert_eq!(TargetHash::parse("a~b").unwrap(), target("", "a", "b"));
        for invalid in ["missing-separator", "a~b~c", "kind#without-tilde"] {
            assert!(TargetHash::parse(invalid).is_err(), "{invalid}");
        }
        assert_eq!(target("Rule", "a", "b").json_value(true), "Rule#a~b");
        assert_eq!(target("Rule", "a", "b").json_value(false), "a~b");
    }

    #[test]
    fn sorts_by_kind_then_label() {
        let from = BTreeMap::new();
        let to = BTreeMap::from([
            ("//z:r".into(), target("Rule", "a", "a")),
            ("//a:g".into(), target("GeneratedFile", "b", "b")),
            ("//b:s".into(), target("SourceFile", "c", "c")),
        ]);
        assert_eq!(
            impacted_targets(&from, &to, None, false).unwrap(),
            ["//b:s", "//a:g", "//z:r"]
        );
    }

    #[test]
    fn computes_shortest_target_and_package_distances_independently() {
        let from = BTreeMap::from([
            ("//a:leaf".into(), target("Rule", "old", "old")),
            ("//a:mid".into(), target("Rule", "old-mid", "same")),
            ("//b:top".into(), target("Rule", "old-top", "same-top")),
        ]);
        let to = BTreeMap::from([
            ("//a:leaf".into(), target("Rule", "new", "new")),
            ("//a:mid".into(), target("Rule", "new-mid", "same")),
            ("//b:top".into(), target("Rule", "new-top", "same-top")),
        ]);
        let edges = BTreeMap::from([
            ("//a:mid".into(), vec!["//a:leaf".into()]),
            ("//b:top".into(), vec!["//a:mid".into()]),
        ]);
        let result = impacted_targets_with_distances(&from, &to, &edges, None, false).unwrap();
        assert_eq!(result[2].target_distance, 2);
        assert_eq!(result[2].package_distance, 1);
    }

    #[test]
    fn streaming_serialization_matches_value_schema() {
        let mut data = HashFileData {
            hashes: BTreeMap::from([
                ("//a:a".into(), target("Rule", "overall-a", "direct-a")),
                (
                    "//b:b".into(),
                    target("SourceFile", "overall-b", "direct-b"),
                ),
            ]),
            ..Default::default()
        };
        for include_target_type in [false, true] {
            let streamed =
                serde_json::to_value(data.serialized(include_target_type, false)).unwrap();
            assert_eq!(streamed, data.to_value(include_target_type, false));
        }

        data.module_graph_json = Some(r#"{"root":"example"}"#.into());
        data.dep_edges = BTreeMap::from([("//a:a".into(), vec!["//b:b".into()])]);
        let streamed = serde_json::to_value(data.serialized(true, true)).unwrap();
        assert_eq!(streamed, data.to_value(true, true));
    }

    #[test]
    fn reads_legacy_and_metadata_hash_files() {
        let legacy = HashFileData::from_slice(br#"{"//a:a":"Rule#overall~direct"}"#).unwrap();
        assert_eq!(legacy.hashes["//a:a"], target("Rule", "overall", "direct"));
        assert!(legacy.module_graph_json.is_none());

        let bytes = br#"{
          "hashes": {"//a:a": "Rule#overall~direct"},
          "metadata": {
            "moduleGraphJson": "{\"root\":true}",
            "depEdges": {"//a:a": ["//b:b"]}
          }
        }"#;
        let parsed = HashFileData::from_slice(bytes).unwrap();
        assert_eq!(parsed.module_graph_json.as_deref(), Some("{\"root\":true}"));
        assert_eq!(parsed.dep_edges["//a:a"], ["//b:b"]);

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        assert_eq!(
            HashFileData::read(file.path()).unwrap().hashes,
            parsed.hashes
        );
    }

    #[test]
    fn rejects_malformed_hash_files() {
        for invalid in [
            b"not json".as_slice(),
            b"[]".as_slice(),
            br#"{"//a:a": 1}"#.as_slice(),
            br#"{"hashes":[],"metadata":{}}"#.as_slice(),
            br#"{"hashes":{},"metadata":{"depEdges":[]}}"#.as_slice(),
        ] {
            assert!(HashFileData::from_slice(invalid).is_err());
        }
        assert!(HashFileData::read(Path::new("/definitely/missing/hash.json")).is_err());
    }

    #[test]
    fn type_filters_require_and_apply_type_metadata() {
        let from = BTreeMap::new();
        let typed = BTreeMap::from([
            ("//a:source".into(), target("SourceFile", "a", "a")),
            ("//b:rule".into(), target("Rule", "b", "b")),
            ("//external:repo".into(), target("Rule", "c", "c")),
        ]);
        let rules = HashSet::from(["Rule".to_owned()]);
        assert_eq!(
            impacted_targets(&from, &typed, Some(&rules), true).unwrap(),
            ["//b:rule"]
        );
        assert_eq!(
            filter_and_sort_labels(
                vec!["//b:rule".into(), "//b:rule".into(), "//a:source".into()],
                &from,
                &typed,
                Some(&rules),
                false,
            )
            .unwrap(),
            ["//b:rule"]
        );

        let untyped = BTreeMap::from([("//a:a".into(), target("", "a", "a"))]);
        assert!(impacted_targets(&from, &untyped, Some(&rules), false).is_err());
        assert!(filter_and_sort_labels(
            vec!["//missing:x".into()],
            &from,
            &typed,
            Some(&rules),
            false,
        )
        .is_err());
    }

    #[test]
    fn unchanged_hashes_and_external_filters_are_excluded() {
        let from = BTreeMap::from([
            ("//a:a".into(), target("Rule", "same", "same")),
            ("//external:r".into(), target("Rule", "old", "old")),
        ]);
        let to = BTreeMap::from([
            ("//a:a".into(), target("Rule", "same", "same")),
            ("//external:r".into(), target("Rule", "new", "new")),
        ]);
        assert!(impacted_targets(&from, &to, None, true).unwrap().is_empty());
        assert_eq!(
            impacted_targets(&from, &to, None, false).unwrap(),
            ["//external:r"]
        );
    }

    #[test]
    fn distances_handle_missing_edges_cycles_and_filters() {
        let from = BTreeMap::from([
            ("//a:direct".into(), target("Rule", "old", "old")),
            ("//b:indirect".into(), target("Rule", "old-b", "same-b")),
            ("//c:cycle".into(), target("Rule", "old-c", "same-c")),
        ]);
        let to = BTreeMap::from([
            ("//a:direct".into(), target("Rule", "new", "new")),
            ("//b:indirect".into(), target("Rule", "new-b", "same-b")),
            ("//c:cycle".into(), target("Rule", "new-c", "same-c")),
            ("//d:added".into(), target("SourceFile", "added", "added")),
        ]);
        let edges = BTreeMap::from([
            ("//b:indirect".into(), vec!["//a:direct".into()]),
            ("//c:cycle".into(), vec!["//c:cycle".into()]),
        ]);
        let source_only = HashSet::from(["SourceFile".to_owned()]);
        let filtered =
            impacted_targets_with_distances(&from, &to, &edges, Some(&source_only), false).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].label, "//d:added");

        let all = impacted_targets_with_distances(&from, &to, &edges, None, false).unwrap();
        let indirect = all
            .iter()
            .find(|target| target.label == "//b:indirect")
            .unwrap();
        assert_eq!(
            (indirect.target_distance, indirect.package_distance),
            (1, 1)
        );
        let cycle = all
            .iter()
            .find(|target| target.label == "//c:cycle")
            .unwrap();
        assert_eq!((cycle.target_distance, cycle.package_distance), (1, 0));
    }
}
