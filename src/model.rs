use anyhow::{anyhow, bail, Context, Result};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path;

/// A target hash as written in a hash file.
///
/// `generate-hashes` writes lowercase hex SHA-256 digests. Those are kept as their 32 raw bytes,
/// so a million-entry file costs one allocation per label instead of three and two entries compare
/// in one fixed-width comparison. Any other string (a hand-written fixture, another producer's
/// digest) is kept verbatim. Either way the value round-trips through `Display` unchanged.
#[derive(Clone, Eq, PartialEq)]
pub enum Digest {
    Sha256([u8; 32]),
    Text(Box<str>),
}

impl Digest {
    pub fn parse(value: &str) -> Self {
        match decode_lowercase_hex(value) {
            Some(bytes) => Self::Sha256(bytes),
            None => Self::Text(value.into()),
        }
    }
}

/// Decodes exactly 64 lowercase hex digits in one table-driven pass; anything else is `None`.
/// This runs twice per entry of every hash file read, so it is the parse's hot loop.
fn decode_lowercase_hex(value: &str) -> Option<[u8; 32]> {
    const INVALID: u8 = 0xff;
    const TABLE: [u8; 256] = {
        let mut table = [INVALID; 256];
        let mut digit = 0;
        while digit < 10 {
            table[b'0' as usize + digit] = digit as u8;
            digit += 1;
        }
        let mut letter = 0;
        while letter < 6 {
            table[b'a' as usize + letter] = 10 + letter as u8;
            letter += 1;
        }
        table
    };
    let hex = value.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (byte, pair) in bytes.iter_mut().zip(hex.chunks_exact(2)) {
        let (high, low) = (TABLE[pair[0] as usize], TABLE[pair[1] as usize]);
        if high == INVALID || low == INVALID {
            return None;
        }
        *byte = (high << 4) | low;
    }
    Some(bytes)
}

impl From<[u8; 32]> for Digest {
    fn from(bytes: [u8; 32]) -> Self {
        Self::Sha256(bytes)
    }
}

impl From<&str> for Digest {
    fn from(value: &str) -> Self {
        Self::parse(value)
    }
}

impl From<String> for Digest {
    fn from(value: String) -> Self {
        Self::parse(&value)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256(bytes) => {
                let mut encoded = [0; 64];
                hex::encode_to_slice(bytes, &mut encoded).expect("fixed-size hex output");
                formatter.write_str(std::str::from_utf8(&encoded).expect("hex output is ASCII"))
            }
            Self::Text(text) => formatter.write_str(text),
        }
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_string(), formatter)
    }
}

impl PartialEq<str> for Digest {
    fn eq(&self, other: &str) -> bool {
        *self == Self::parse(other)
    }
}

impl PartialEq<&str> for Digest {
    fn eq(&self, other: &&str) -> bool {
        *self == Self::parse(other)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetHash {
    pub kind: String,
    pub hash: Digest,
    pub direct_hash: Digest,
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
            hash: Digest::parse(hash),
            direct_hash: Digest::parse(direct_hash),
        })
    }

    /// Whether `other` records the same state for a target: same kind and same two hashes. This
    /// is the whole comparison `get-impacted-targets` makes per label, so it allocates nothing.
    pub fn same_as(&self, other: &Self) -> bool {
        self.hash == other.hash && self.direct_hash == other.direct_hash && self.kind == other.kind
    }

    pub fn json_value(&self, include_target_type: bool) -> String {
        if include_target_type {
            format!("{}#{}~{}", self.kind, self.hash, self.direct_hash)
        } else {
            format!("{}~{}", self.hash, self.direct_hash)
        }
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
            usize::from(self.data.module_graph_json.is_some())
                + usize::from(include_dep_edges)
                + usize::from(self.data.dependency_fingerprint.is_some()),
        )?;
        if let Some(module_graph_json) = &self.data.module_graph_json {
            state.serialize_field("moduleGraphJson", module_graph_json)?;
        }
        if let Some(dependency_fingerprint) = &self.data.dependency_fingerprint {
            state.serialize_field("dependencyFingerprint", dependency_fingerprint)?;
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
            && self.data.dependency_fingerprint.is_none()
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
    pub dependency_fingerprint: Option<String>,
    pub dep_edges: BTreeMap<String, Vec<String>>,
}

/// A JSON string borrowed from the input when it has no escapes, copied otherwise. Hash values
/// are parsed and dropped right away, so borrowing saves one allocation per entry.
struct JsonStr<'de>(Cow<'de, str>);

impl<'de> Deserialize<'de> for JsonStr<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonStrVisitor;

        impl<'de> Visitor<'de> for JsonStrVisitor {
            type Value = JsonStr<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
                Ok(JsonStr(Cow::Borrowed(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(JsonStr(Cow::Owned(value.to_owned())))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(JsonStr(Cow::Owned(value)))
            }
        }

        deserializer.deserialize_str(JsonStrVisitor)
    }
}

/// The `{label: "kind#hash~direct"}` object, parsed entry by entry.
struct Hashes(BTreeMap<String, TargetHash>);

impl<'de> Deserialize<'de> for Hashes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HashesVisitor;

        impl<'de> Visitor<'de> for HashesVisitor {
            type Value = Hashes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object mapping labels to hash strings")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut hashes = BTreeMap::new();
                while let Some((label, value)) = map.next_entry::<String, JsonStr<'de>>()? {
                    let hash = TargetHash::parse(&value.0).map_err(de::Error::custom)?;
                    hashes.insert(label, hash);
                }
                Ok(Hashes(hashes))
            }
        }

        deserializer.deserialize_map(HashesVisitor)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    module_graph_json: Option<String>,
    dependency_fingerprint: Option<String>,
    #[serde(default)]
    dep_edges: BTreeMap<String, Vec<String>>,
}

impl<'de> Deserialize<'de> for HashFileData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HashFileVisitor;

        impl<'de> Visitor<'de> for HashFileVisitor {
            type Value = HashFileData;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a hash file object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut data = HashFileData::default();
                // Labels always start with `//`, `@` or a repository name, so the two wrapper
                // keys cannot collide with one.
                while let Some(key) = map.next_key::<JsonStr<'de>>()? {
                    match &*key.0 {
                        "hashes" => data.hashes = map.next_value::<Hashes>()?.0,
                        "metadata" => {
                            let metadata = map.next_value::<Metadata>()?;
                            data.module_graph_json = metadata.module_graph_json;
                            data.dependency_fingerprint = metadata.dependency_fingerprint;
                            data.dep_edges = metadata.dep_edges;
                        }
                        label => {
                            let value = map.next_value::<JsonStr<'de>>()?;
                            let hash = TargetHash::parse(&value.0).map_err(de::Error::custom)?;
                            data.hashes.insert(label.to_owned(), hash);
                        }
                    }
                }
                Ok(data)
            }
        }

        deserializer.deserialize_map(HashFileVisitor)
    }
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

    /// Parses either hash-file shape: a bare `{label: hash}` object, or `{"hashes": {...},
    /// "metadata": {...}}`. Entries go straight from the JSON tokens into the final map, with no
    /// intermediate `serde_json::Value`, which is what keeps a million-entry file to one pass and
    /// one allocation per label.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("invalid hash JSON")
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
        if self.module_graph_json.is_none()
            && self.dependency_fingerprint.is_none()
            && (!include_deps || self.dep_edges.is_empty())
        {
            return Value::Object(hashes);
        }
        let mut metadata = Map::new();
        if let Some(module_graph_json) = &self.module_graph_json {
            metadata.insert(
                "moduleGraphJson".to_owned(),
                Value::String(module_graph_json.clone()),
            );
        }
        if let Some(dependency_fingerprint) = &self.dependency_fingerprint {
            metadata.insert(
                "dependencyFingerprint".to_owned(),
                Value::String(dependency_fingerprint.clone()),
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

/// How a label's hash moved between two revisions: its own `directHash`
/// changed (or it is new), or only its transitive hash did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImpactType {
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
        .filter(|(label, hash)| {
            !from
                .get(*label)
                .is_some_and(|previous| previous.same_as(hash))
        })
        .map(|(label, _)| label.clone())
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
    // Rank once per label and sort the pairs: the rank is a function of the label, so deduping the
    // pairs dedups the labels, and nothing is cloned or looked up inside the sort.
    let mut ranked = impacted
        .drain(..)
        .map(|label| {
            let rank = kind_rank(to.get(&label).or_else(|| from.get(&label)));
            (rank, label)
        })
        .collect::<Vec<_>>();
    ranked.sort();
    ranked.dedup();
    Ok(ranked.into_iter().map(|(_, label)| label).collect())
}

/// Classifies every label whose hash moved between `from` and `to`. Labels
/// absent from `to` (deleted targets) are never impacted: there is nothing
/// left to build.
pub fn impacted_types(
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
) -> HashMap<String, ImpactType> {
    to.iter()
        .filter_map(|(label, final_hash)| match from.get(label) {
            None => Some((label.clone(), ImpactType::Direct)),
            Some(starting_hash) if !starting_hash.same_as(final_hash) => Some((
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
            hash: hash.into(),
            direct_hash: direct.into(),
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

        data.module_graph_json = None;
        data.dep_edges.clear();
        data.dependency_fingerprint = Some("dep-fp".into());
        let streamed = serde_json::to_value(data.serialized(true, false)).unwrap();
        assert_eq!(streamed, data.to_value(true, false));
        assert!(streamed.get("metadata").is_some());
    }

    #[test]
    fn reads_legacy_and_metadata_hash_files() {
        let legacy = HashFileData::from_slice(br#"{"//a:a":"Rule#overall~direct"}"#).unwrap();
        assert_eq!(legacy.hashes["//a:a"], target("Rule", "overall", "direct"));
        assert!(legacy.module_graph_json.is_none());
        assert!(legacy.dependency_fingerprint.is_none());

        let bytes = br#"{
          "hashes": {"//a:a": "Rule#overall~direct"},
          "metadata": {
            "moduleGraphJson": "{\"root\":true}",
            "dependencyFingerprint": "dep-fp",
            "depEdges": {"//a:a": ["//b:b"]}
          }
        }"#;
        let parsed = HashFileData::from_slice(bytes).unwrap();
        assert_eq!(parsed.module_graph_json.as_deref(), Some("{\"root\":true}"));
        assert_eq!(parsed.dependency_fingerprint.as_deref(), Some("dep-fp"));
        assert_eq!(parsed.dep_edges["//a:a"], ["//b:b"]);

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        assert_eq!(
            HashFileData::read(file.path()).unwrap().hashes,
            parsed.hashes
        );
    }

    #[test]
    fn digests_keep_sha256_as_bytes_and_everything_else_verbatim() {
        let hex = "0123456789abcdef".repeat(4);
        let digest = Digest::parse(&hex);
        assert!(matches!(digest, Digest::Sha256(_)));
        assert_eq!(digest.to_string(), hex);
        assert_eq!(digest, hex.as_str());
        assert_eq!(format!("{digest:?}"), format!("{hex:?}"));
        assert_eq!(Digest::from(hex.clone()), digest);
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef].repeat(4);
        assert_eq!(Digest::from(<[u8; 32]>::try_from(bytes).unwrap()), digest);

        // Uppercase hex, the wrong width, and fixtures stay text and round-trip unchanged.
        for text in [
            hex.to_uppercase(),
            hex[..63].to_owned(),
            "overall".to_owned(),
        ] {
            let digest = Digest::parse(&text);
            assert!(matches!(digest, Digest::Text(_)), "{text}");
            assert_eq!(digest.to_string(), text);
            assert_eq!(digest, text.as_str());
            assert_ne!(digest, hex.as_str());
        }
    }

    #[test]
    fn same_as_compares_kind_and_both_hashes() {
        let base = target("Rule", "a", "b");
        assert!(base.same_as(&target("Rule", "a", "b")));
        assert!(!base.same_as(&target("SourceFile", "a", "b")));
        assert!(!base.same_as(&target("Rule", "x", "b")));
        assert!(!base.same_as(&target("Rule", "a", "x")));
    }

    #[test]
    fn direct_deserialization_handles_escapes_wrappers_and_real_digests() {
        let hex = "0123456789abcdef".repeat(4);
        // An escaped label and value take the owned path; the rest borrow from the input.
        let bytes = format!(r#"{{"//a:a":"Rule#{hex}~{hex}","//b:\u0062":"Source\u0046ile#x~y"}}"#);
        let parsed = HashFileData::from_slice(bytes.as_bytes()).unwrap();
        assert_eq!(parsed.hashes["//a:a"], target("Rule", &hex, &hex));
        assert!(matches!(parsed.hashes["//a:a"].hash, Digest::Sha256(_)));
        assert_eq!(parsed.hashes["//b:b"], target("SourceFile", "x", "y"));
        assert_eq!(
            serde_json::to_value(parsed.serialized(true, false)).unwrap(),
            serde_json::json!({"//a:a": format!("Rule#{hex}~{hex}"), "//b:b": "SourceFile#x~y"})
        );

        // A wrapper without metadata, and metadata before hashes, both read.
        let wrapped = HashFileData::from_slice(br#"{"hashes":{"//a:a":"a~b"}}"#).unwrap();
        assert_eq!(wrapped.hashes["//a:a"], target("", "a", "b"));
        let reordered = HashFileData::from_slice(
            br#"{"metadata":{"dependencyFingerprint":"fp","unknown":1},"hashes":{"//a:a":"a~b"}}"#,
        )
        .unwrap();
        assert_eq!(reordered.dependency_fingerprint.as_deref(), Some("fp"));
        assert_eq!(reordered.hashes.len(), 1);
        assert!(reordered.dep_edges.is_empty());
    }

    #[test]
    fn rejects_malformed_hash_files() {
        for invalid in [
            b"not json".as_slice(),
            b"[]".as_slice(),
            br#"{"//a:a": 1}"#.as_slice(),
            br#"{"hashes":[],"metadata":{}}"#.as_slice(),
            br#"{"hashes":{},"metadata":{"depEdges":[]}}"#.as_slice(),
            br#"{"//a:a": "no-tilde"}"#.as_slice(),
            br#"{"hashes":{"//a:a": "a~b~c"}}"#.as_slice(),
            br#"{"hashes":{},"metadata":{"moduleGraphJson":1}}"#.as_slice(),
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

    /// An indirectly impacted target whose dep edges name no impacted dependency (a stale or
    /// partial dep-edges file, #268) cannot be placed: it is reported at distance 0 rather than
    /// dropped or left at `usize::MAX`, and the fallback is memoised like any other answer so a
    /// dependent is one hop past it.
    #[test]
    fn unattributable_indirect_impact_falls_back_to_distance_zero() {
        let from = BTreeMap::from([
            ("//a:orphan".into(), target("Rule", "old", "same")),
            ("//b:dependent".into(), target("Rule", "old-b", "same-b")),
        ]);
        let to = BTreeMap::from([
            ("//a:orphan".into(), target("Rule", "new", "same")),
            ("//b:dependent".into(), target("Rule", "new-b", "same-b")),
        ]);
        // The orphan's only edge points at an unimpacted target; the dependent reaches the
        // orphan through it.
        let edges = BTreeMap::from([
            ("//a:orphan".into(), vec!["//c:unchanged".into()]),
            ("//b:dependent".into(), vec!["//a:orphan".into()]),
        ]);
        let result = impacted_targets_with_distances(&from, &to, &edges, None, false).unwrap();
        assert_eq!(
            result,
            [
                ImpactedTargetWithDistance {
                    label: "//a:orphan".into(),
                    target_distance: 0,
                    package_distance: 0,
                },
                ImpactedTargetWithDistance {
                    label: "//b:dependent".into(),
                    target_distance: 1,
                    package_distance: 1,
                },
            ]
        );

        // The same without any edges at all: every indirect impact is unattributable.
        let result =
            impacted_targets_with_distances(&from, &to, &BTreeMap::new(), None, false).unwrap();
        assert!(
            result
                .iter()
                .all(|target| (target.target_distance, target.package_distance) == (0, 0)),
            "{result:?}"
        );
    }

    /// A target deleted between the revisions has nothing left to build, so it is never
    /// impacted, with or without distances.
    #[test]
    fn deleted_targets_are_not_impacted() {
        let from = BTreeMap::from([
            ("//a:kept".into(), target("Rule", "same", "same")),
            ("//a:deleted".into(), target("Rule", "gone", "gone")),
        ]);
        let to = BTreeMap::from([("//a:kept".into(), target("Rule", "same", "same"))]);
        assert!(impacted_types(&from, &to).is_empty());
        assert!(impacted_targets(&from, &to, None, false)
            .unwrap()
            .is_empty());
        assert!(
            impacted_targets_with_distances(&from, &to, &BTreeMap::new(), None, false)
                .unwrap()
                .is_empty()
        );
    }

    /// Ordering puts source files first, then generated files, then rules; any other kind
    /// sorts after those, and a label known to neither revision sorts last. Within one rank
    /// labels are alphabetical, so a `//z` source file still precedes an `//a` rule.
    #[test]
    fn unknown_kinds_and_unknown_labels_sort_after_the_known_kinds() {
        let from = BTreeMap::from([("//from:only".into(), target("Rule", "a", "a"))]);
        let to = BTreeMap::from([
            ("//z:source".into(), target("SourceFile", "a", "a")),
            ("//m:rule".into(), target("Rule", "a", "a")),
            ("//a:package_group".into(), target("PackageGroup", "a", "a")),
            ("//a:untyped".into(), target("", "a", "a")),
        ]);
        let labels = [
            "//nowhere:label",
            "//a:untyped",
            "//a:package_group",
            "//m:rule",
            "//z:source",
            "//from:only",
        ]
        .map(str::to_owned);
        assert_eq!(
            filter_and_sort_labels(labels, &from, &to, None, false).unwrap(),
            [
                "//z:source",
                "//from:only",
                "//m:rule",
                "//a:package_group",
                "//a:untyped",
                "//nowhere:label",
            ]
        );

        let from = BTreeMap::new();
        let distances = impacted_targets_with_distances(&from, &to, &BTreeMap::new(), None, false)
            .unwrap()
            .into_iter()
            .map(|target| target.label)
            .collect::<Vec<_>>();
        assert_eq!(
            distances,
            ["//z:source", "//m:rule", "//a:package_group", "//a:untyped"]
        );
    }
}
