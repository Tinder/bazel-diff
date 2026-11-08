use crate::partial::PartialRecipes;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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
    include_target_type: bool,
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
            usize::from(self.data.observed_toolchains.is_some())
                + usize::from(self.data.module_graph_json.is_some())
                + usize::from(include_dep_edges)
                + usize::from(self.data.dependency_fingerprint.is_some())
                + usize::from(self.data.partial.is_some())
                + usize::from(self.data.tree.is_some())
                + usize::from(self.data.configured_roots.is_some()),
        )?;
        if let Some(index) = &self.data.observed_toolchains {
            state.serialize_field("observedToolchains", index)?;
        }
        if let Some(module_graph_json) = &self.data.module_graph_json {
            state.serialize_field("moduleGraphJson", module_graph_json)?;
        }
        if let Some(dependency_fingerprint) = &self.data.dependency_fingerprint {
            state.serialize_field("dependencyFingerprint", dependency_fingerprint)?;
        }
        if include_dep_edges {
            state.serialize_field("depEdges", &self.data.dep_edges)?;
        }
        if let Some(partial) = &self.data.partial {
            state.serialize_field("partialGraph", partial)?;
        }
        if let Some(roots) = &self.data.configured_roots {
            state.serialize_field(
                "configuredRoots",
                &roots.serialized(self.include_target_type),
            )?;
        }
        if let Some(tree) = &self.data.tree {
            state.serialize_field("tree", tree)?;
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
        if self.data.observed_toolchains.is_none()
            && self.data.module_graph_json.is_none()
            && self.data.dependency_fingerprint.is_none()
            && self.data.partial.is_none()
            && self.data.configured_roots.is_none()
            && self.data.tree.is_none()
            && (!self.include_deps || self.data.dep_edges.is_empty())
        {
            return hashes.serialize(serializer);
        }
        let mut state = serializer.serialize_struct("HashFileData", 2)?;
        state.serialize_field("hashes", &hashes)?;
        state.serialize_field(
            "metadata",
            &SerializedMetadata {
                include_target_type: self.include_target_type,
                data: self.data,
                include_deps: self.include_deps,
            },
        )?;
        state.end()
    }
}

#[derive(Clone, Debug, Default)]
pub struct HashFileData {
    pub observed_toolchains: Option<crate::toolchain_candidates::ObservedToolchains>,
    pub configured_roots: Option<ConfiguredRoots>,
    pub hashes: BTreeMap<String, TargetHash>,
    pub module_graph_json: Option<String>,
    pub dependency_fingerprint: Option<String>,
    pub dep_edges: BTreeMap<String, Vec<String>>,
    /// Partial-graph mode (`--partialGraph`): the recipes a derivation re-evaluates.
    pub partial: Option<PartialRecipes>,
    /// Partial-graph mode: the id of the git tree the hashes describe, the base of a later
    /// derivation's `git diff`.
    pub tree: Option<String>,
}

/// An explicitly requested configured-root universe. The public hashes are the scheduled
/// roots. These additional hashes retain the configured dependency identities for attribution.
#[derive(Clone, Debug, Default)]
pub struct ConfiguredRoots {
    pub roots: BTreeMap<String, String>,
    pub hashes: BTreeMap<String, TargetHash>,
    /// Requested roots whose fresh verification view was completely represented in these hashes,
    /// mapped to a digest of additional observed context. Absence is the historical format and
    /// carries no negative authority; the comparator still has to establish equality.
    pub verified_roots: Option<BTreeMap<String, String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SerializedConfiguredRoots<'a> {
    version: u32,
    roots: &'a BTreeMap<String, String>,
    hashes: SerializedHashes<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified_roots: Option<&'a BTreeMap<String, String>>,
}

impl ConfiguredRoots {
    /// Version 1 could combine distinct toolchains with the same public cquery key.
    pub const VERSION: u32 = 2;

    fn serialized(&self, include_target_type: bool) -> SerializedConfiguredRoots<'_> {
        SerializedConfiguredRoots {
            version: Self::VERSION,
            roots: &self.roots,
            hashes: SerializedHashes {
                hashes: &self.hashes,
                include_target_type,
            },
            verified_roots: self.verified_roots.as_ref(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfiguredRootsWire {
    version: u32,
    roots: BTreeMap<String, String>,
    hashes: Hashes,
    #[serde(default)]
    verified_roots: Option<BTreeMap<String, String>>,
}

impl<'de> Deserialize<'de> for ConfiguredRoots {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ConfiguredRootsWire::deserialize(deserializer)?;
        if wire.version != Self::VERSION {
            return Err(de::Error::custom(format!(
                "unsupported configuredRoots version {}",
                wire.version
            )));
        }
        if wire.verified_roots.as_ref().is_some_and(|verified| {
            verified.iter().any(|(label, context)| {
                !wire.roots.contains_key(label)
                    || context.len() != 64
                    || !context.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        }) {
            return Err(de::Error::custom(
                "verified configured root has an invalid context or is outside the requested root set",
            ));
        }
        Ok(Self {
            roots: wire.roots,
            hashes: wire.hashes.0,
            verified_roots: wire.verified_roots,
        })
    }
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

/// Parses a `--depEdgesFile` (`{label: [dependency labels]}`). `generate-hashes` writes it from a
/// sorted map, so the entries are collected and the map built in one bulk pass, as for [`Hashes`],
/// instead of searching the tree for every label; a repeated label keeps its last value, as
/// deserializing straight into a `BTreeMap` does.
pub fn dep_edges_from_slice(bytes: &[u8]) -> serde_json::Result<BTreeMap<String, Vec<String>>> {
    struct DepEdges(BTreeMap<String, Vec<String>>);

    impl<'de> Deserialize<'de> for DepEdges {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct DepEdgesVisitor;

            impl<'de> Visitor<'de> for DepEdgesVisitor {
                type Value = DepEdges;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("an object mapping labels to label arrays")
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                    while let Some(entry) = map.next_entry::<String, Vec<String>>()? {
                        entries.push(entry);
                    }
                    Ok(DepEdges(entries.into_iter().collect()))
                }
            }

            deserializer.deserialize_map(DepEdgesVisitor)
        }
    }

    serde_json::from_slice::<DepEdges>(bytes).map(|edges| edges.0)
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
                // `generate-hashes` writes labels in sorted order, so collecting the entries and
                // building the map in one bulk pass replaces a root-to-leaf search (a string
                // comparison per level) for every insert. `BTreeMap::from_iter` sorts stably and
                // keeps the last value for a repeated label, exactly as repeated inserts would.
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((label, value)) = map.next_entry::<String, JsonStr<'de>>()? {
                    let hash = TargetHash::parse(&value.0).map_err(de::Error::custom)?;
                    entries.push((label, hash));
                }
                Ok(Hashes(entries.into_iter().collect()))
            }
        }

        deserializer.deserialize_map(HashesVisitor)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    observed_toolchains: Option<crate::toolchain_candidates::ObservedToolchains>,
    configured_roots: Option<ConfiguredRoots>,
    module_graph_json: Option<String>,
    dependency_fingerprint: Option<String>,
    #[serde(default)]
    dep_edges: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    partial_graph: Option<PartialRecipes>,
    #[serde(default)]
    tree: Option<String>,
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
                // Entries of the bare shape, buffered for one bulk build like `Hashes` does. They
                // all follow the last `"hashes"` key (which replaces everything before it), so
                // they are applied last, each overriding an earlier value for its label.
                let mut bare = Vec::new();
                // Labels always start with `//`, `@` or a repository name, so the two wrapper
                // keys cannot collide with one.
                while let Some(key) = map.next_key::<JsonStr<'de>>()? {
                    match &*key.0 {
                        "hashes" => {
                            bare.clear();
                            data.hashes = map.next_value::<Hashes>()?.0;
                        }
                        "metadata" => {
                            let metadata = map.next_value::<Metadata>()?;
                            data.observed_toolchains = metadata.observed_toolchains;
                            data.module_graph_json = metadata.module_graph_json;
                            data.dependency_fingerprint = metadata.dependency_fingerprint;
                            data.dep_edges = metadata.dep_edges;
                            data.configured_roots = metadata.configured_roots;
                            data.partial = metadata.partial_graph;
                            data.tree = metadata.tree;
                        }
                        label => {
                            let value = map.next_value::<JsonStr<'de>>()?;
                            let hash = TargetHash::parse(&value.0).map_err(de::Error::custom)?;
                            bare.push((label.to_owned(), hash));
                        }
                    }
                }
                if data.hashes.is_empty() {
                    data.hashes = bare.into_iter().collect();
                } else {
                    data.hashes.extend(bare);
                }
                if let Some(roots) = &data.configured_roots {
                    if data.partial.is_some()
                        || roots.roots.len() != data.hashes.len()
                        || roots.roots.iter().any(|(label, key)| {
                            match (data.hashes.get(label), roots.hashes.get(key)) {
                                (Some(root), Some(instance)) => root != instance,
                                _ => true,
                            }
                        })
                    {
                        return Err(de::Error::custom("configuredRoots roster does not match the scheduled hashes and their instances"));
                    }
                }
                Ok(data)
            }
        }

        deserializer.deserialize_map(HashFileVisitor)
    }
}

impl HashFileData {
    pub fn ensure_same_root_mode(&self, other: &Self) -> Result<()> {
        match (&self.configured_roots, &other.configured_roots) {
            (None, None) => Ok(()),
            (Some(_), Some(_)) => Ok(()),
            _ => bail!("incompatible configured-root hash files; generate both in the same mode"),
        }
    }

    /// Consume the two domains for explaining a single requested target. A scheduled
    /// root uses its own endpoint's hash even when its selected configuration changes.
    pub fn into_explain_hashes(self, target: &str) -> BTreeMap<String, TargetHash> {
        let Some(roots) = self.configured_roots else {
            return self.hashes;
        };
        let mut graph = roots.hashes;
        if !self.hashes.contains_key(target) {
            graph.remove(target);
        }
        graph.extend(self.hashes);
        graph
    }

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
        if self.observed_toolchains.is_none()
            && self.module_graph_json.is_none()
            && self.dependency_fingerprint.is_none()
            && self.partial.is_none()
            && self.configured_roots.is_none()
            && self.tree.is_none()
            && (!include_deps || self.dep_edges.is_empty())
        {
            return Value::Object(hashes);
        }
        let mut metadata = Map::new();
        if let Some(index) = &self.observed_toolchains {
            metadata.insert(
                "observedToolchains".into(),
                serde_json::to_value(index)
                    .expect("serializing toolchain observations cannot fail"),
            );
        }
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
        if let Some(partial) = &self.partial {
            metadata.insert(
                "partialGraph".to_owned(),
                serde_json::to_value(partial).expect("serializing recipes cannot fail"),
            );
        }
        if let Some(roots) = &self.configured_roots {
            metadata.insert(
                "configuredRoots".to_owned(),
                serde_json::to_value(roots.serialized(include_target_type))
                    .expect("serializing string map cannot fail"),
            );
        }
        if let Some(tree) = &self.tree {
            metadata.insert("tree".to_owned(), Value::String(tree.clone()));
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

/// The native comparison result and the changed graph used to compute its distances.
/// Read each path's keys in the dependency-edges file supplied to this comparison.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ImpactGraph {
    pub version: u32,
    pub targets: Vec<ImpactedTargetWithDistance>,
    pub paths: Vec<ImpactPath>,
    /// Roots whose verified configured hash inputs and context are equal in this comparison.
    /// An absent label remains unknown; caller obligations outside this graph still apply.
    pub unaffected_roots: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ImpactPathKind {
    Ordinary,
    Configured,
    Shared,
    /// An independent MODULE obligation, represented as one distance-zero root.
    Forced,
}

/// A set of eligible outputs and the native origins and changed nodes that reach them.
/// `direct` includes the existing distance-zero fallback for incomplete edge files.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ImpactPath {
    pub kind: ImpactPathKind,
    pub changed: BTreeSet<String>,
    pub direct: BTreeSet<String>,
    /// Public output label to its key in the supplied head dependency graph.
    pub roots: BTreeMap<String, String>,
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

/// Reverse 0/1 BFS. Unit edge cost gives target hops; package-change cost gives the independent
/// minimum number of package crossings.
fn reverse_distances<'a>(
    dependents: &HashMap<&'a str, Vec<&'a str>>,
    roots: &[&'a str],
    edge_cost: impl Fn(&str, &str) -> Option<usize>,
) -> HashMap<&'a str, usize> {
    let mut distances = roots
        .iter()
        .map(|&root| (root, 0_usize))
        .collect::<HashMap<_, _>>();
    let mut queue = roots
        .iter()
        .map(|&root| (root, 0_usize))
        .collect::<VecDeque<_>>();
    while let Some((label, distance)) = queue.pop_front() {
        if distances.get(label) != Some(&distance) {
            continue;
        }
        for &dependent in dependents.get(label).into_iter().flatten() {
            let Some(cost) = edge_cost(label, dependent) else {
                continue;
            };
            debug_assert!(cost <= 1);
            let candidate = distance.saturating_add(cost);
            let improved = match distances.entry(dependent) {
                Entry::Vacant(entry) => {
                    entry.insert(candidate);
                    true
                }
                Entry::Occupied(mut entry) => {
                    if candidate < *entry.get() {
                        entry.insert(candidate);
                        true
                    } else {
                        false
                    }
                }
            };
            if improved {
                if cost == 0 {
                    queue.push_front((dependent, candidate));
                } else {
                    queue.push_back((dependent, candidate));
                }
            }
        }
    }
    distances
}

pub fn impacted_targets_with_distances(
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
    dep_edges: &BTreeMap<String, Vec<String>>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
) -> Result<Vec<ImpactedTargetWithDistance>> {
    let impact_types = impacted_types(from, to);
    let result = distances_for_types(
        from,
        to,
        dep_edges,
        target_types,
        exclude_external,
        impact_types,
        DistanceOptions::default(),
    )?;
    Ok(result.targets)
}

/// Walk the configured cause graph before selecting the requested outputs. A source
/// newly listed as a root can already exist as an unchanged child in both graphs.
pub fn hash_file_distances(
    from: &HashFileData,
    to: &HashFileData,
    dep_edges: &BTreeMap<String, Vec<String>>,
    module_impacted: Option<&std::collections::BTreeSet<String>>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
) -> Result<Vec<ImpactedTargetWithDistance>> {
    let result = hash_file_distance_results(
        from,
        to,
        dep_edges,
        module_impacted,
        target_types,
        exclude_external,
        DistanceRequest::default(),
    )?;
    Ok(result.targets)
}

/// Return the native paths for a standalone ordinary or configured-root comparison.
/// Enrollment alone does not establish a change for a configured root first requested at the
/// head. Such a root needs a shared direct change or an independent MODULE obligation.
pub(crate) fn hash_file_impact_graph(
    from: &HashFileData,
    to: &HashFileData,
    dep_edges: &BTreeMap<String, Vec<String>>,
    module_forced: &BTreeSet<String>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
) -> Result<ImpactGraph> {
    let (requested, shared) = from
        .configured_roots
        .as_ref()
        .zip(to.configured_roots.as_ref())
        .map(|(before, after)| {
            let requested = before
                .roots
                .keys()
                .map(|label| crate::bazel::main_repo_label_key(label))
                .collect::<HashSet<_>>();
            let shared = after
                .roots
                .keys()
                .any(|label| !requested.contains(crate::bazel::main_repo_label_key(label)))
                .then(|| shared_direct_changes(before, after))
                .filter(|origins| !origins.is_empty());
            (Some(requested), shared)
        })
        .unwrap_or_default();
    // MODULE obligations are emitted separately and cannot become path origins for other roots.
    let mut result = hash_file_distance_results(
        from,
        to,
        dep_edges,
        None,
        target_types,
        exclude_external,
        DistanceRequest {
            shared: shared.as_ref(),
            capture: true,
        },
    )?;
    if let Some(requested) = &requested {
        result
            .targets
            .retain(|target| requested.contains(crate::bazel::main_repo_label_key(&target.label)));
        result
            .shared
            .retain(|target| !requested.contains(crate::bazel::main_repo_label_key(&target.label)));
    }
    let trace = result.trace.expect("capture requested");
    result
        .targets
        .retain(|target| trace.changed.contains(&target.label));
    let graph_roots = result
        .targets
        .iter()
        .map(|target| (target.label.clone(), target.label.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut paths = Vec::new();
    if !graph_roots.is_empty() {
        if !trace.direct.is_subset(&trace.changed)
            || graph_roots.values().any(|key| !trace.changed.contains(key))
        {
            bail!("impact path origin or root is outside its changed graph");
        }
        paths.push(ImpactPath {
            kind: if to.configured_roots.is_some() {
                ImpactPathKind::Configured
            } else {
                ImpactPathKind::Ordinary
            },
            changed: trace.changed,
            direct: trace.direct,
            roots: graph_roots,
        });
    }
    let mut needs_sort = false;
    if !result.shared.is_empty() {
        let roots = result
            .shared
            .iter()
            .map(|target| (target.label.clone(), target.label.clone()))
            .collect::<BTreeMap<_, _>>();
        if !trace.shared_direct.is_subset(&trace.shared_changed)
            || roots
                .values()
                .any(|key| !trace.shared_changed.contains(key))
        {
            bail!("shared impact path origin or root is outside its changed graph");
        }
        paths.push(ImpactPath {
            kind: ImpactPathKind::Shared,
            changed: trace.shared_changed,
            direct: trace.shared_direct,
            roots,
        });
        result.targets.extend(result.shared);
        needs_sort = true;
    }
    let mut forced_roots = BTreeMap::new();
    for label in module_forced {
        if to.hashes.contains_key(label)
            && (!exclude_external || !label.starts_with("//external:"))
            && accepts(label, &to.hashes, target_types)?
        {
            forced_roots.insert(label.clone(), label.clone());
        }
    }
    if !forced_roots.is_empty() {
        let positions = result
            .targets
            .iter()
            .enumerate()
            .map(|(index, target)| (target.label.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut updates = Vec::new();
        let mut additions = Vec::new();
        for label in forced_roots.keys() {
            if let Some(index) = positions.get(label.as_str()) {
                updates.push(*index);
            } else {
                additions.push(ImpactedTargetWithDistance {
                    label: label.clone(),
                    target_distance: 0,
                    package_distance: 0,
                });
            }
        }
        for index in updates {
            result.targets[index].target_distance = 0;
            result.targets[index].package_distance = 0;
        }
        needs_sort |= !additions.is_empty();
        result.targets.extend(additions);
    }
    for (label, key) in forced_roots {
        let changed = BTreeSet::from([key.clone()]);
        paths.push(ImpactPath {
            kind: ImpactPathKind::Forced,
            changed: changed.clone(),
            direct: changed,
            roots: BTreeMap::from([(label, key)]),
        });
    }
    if needs_sort {
        let mut ranked = result
            .targets
            .into_iter()
            .map(|target| {
                let rank = kind_rank(
                    to.hashes
                        .get(&target.label)
                        .or_else(|| from.hashes.get(&target.label)),
                );
                (rank, target)
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|(left_rank, left), (right_rank, right)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| left.label.cmp(&right.label))
        });
        result.targets = ranked.into_iter().map(|(_, target)| target).collect();
    }
    let unaffected_roots =
        verified_unaffected_roots(from, to, module_forced, &result.targets, target_types);
    Ok(ImpactGraph {
        version: 1,
        targets: result.targets,
        paths,
        unaffected_roots,
    })
}

fn verified_unaffected_roots(
    from: &HashFileData,
    to: &HashFileData,
    module_forced: &BTreeSet<String>,
    targets: &[ImpactedTargetWithDistance],
    target_types: Option<&HashSet<String>>,
) -> BTreeSet<String> {
    if from.partial.is_some()
        || to.partial.is_some()
        || !from
            .module_graph_json
            .as_ref()
            .zip(to.module_graph_json.as_ref())
            .is_some_and(|(before, after)| {
                before == after && crate::module_graph::usable_module_graph(before)
            })
    {
        return BTreeSet::new();
    }
    let Some((before, after)) = from
        .configured_roots
        .as_ref()
        .zip(to.configured_roots.as_ref())
    else {
        return BTreeSet::new();
    };
    let Some((before_verified, after_verified)) = before
        .verified_roots
        .as_ref()
        .zip(after.verified_roots.as_ref())
    else {
        return BTreeSet::new();
    };
    let positive = targets
        .iter()
        .map(|target| target.label.as_str())
        .chain(module_forced.iter().map(String::as_str))
        .map(crate::bazel::main_repo_label_key)
        .collect::<HashSet<_>>();
    after_verified
        .iter()
        .filter(|(label, context)| {
            let normalized = crate::bazel::main_repo_label_key(label);
            if !normalized.starts_with("//")
                || normalized.starts_with("//external:")
                || before_verified.get(*label) != Some(*context)
                || positive.contains(normalized)
                || target_types.is_some_and(|types| {
                    !to.hashes
                        .get(*label)
                        .is_some_and(|hash| types.contains(&hash.kind))
                })
            {
                return false;
            }
            let Some(key) = before
                .roots
                .get(*label)
                .zip(after.roots.get(*label))
                .filter(|(before, after)| before == after)
                .map(|(_, after)| after)
            else {
                return false;
            };
            before
                .hashes
                .get(key)
                .zip(after.hashes.get(key))
                .is_some_and(|(before, after)| before.same_as(after))
                && from
                    .hashes
                    .get(*label)
                    .zip(to.hashes.get(*label))
                    .is_some_and(|(before, after)| before.same_as(after))
        })
        .map(|(label, _)| label.clone())
        .collect()
}

fn shared_direct_changes(before: &ConfiguredRoots, after: &ConfiguredRoots) -> HashSet<String> {
    let mut changed = after
        .hashes
        .iter()
        .filter(|(key, hash)| {
            !key.starts_with("//external:")
                && before
                    .hashes
                    .get(*key)
                    .is_some_and(|old| old.direct_hash != hash.direct_hash)
        })
        .map(|(key, _)| key.clone())
        .collect::<HashSet<_>>();
    // A root view projects the instance's inputs instead of adding an edge to the instance.
    // Preserve a direct change on the shared instance itself as a direct root-view origin.
    for (root, key) in &after.roots {
        if changed.contains(key) {
            changed.insert(root.clone());
        }
    }
    changed
}

struct DistanceResults {
    targets: Vec<ImpactedTargetWithDistance>,
    shared: Vec<ImpactedTargetWithDistance>,
    trace: Option<DistanceTrace>,
}

struct DistanceTrace {
    changed: BTreeSet<String>,
    direct: BTreeSet<String>,
    shared_changed: BTreeSet<String>,
    shared_direct: BTreeSet<String>,
}

#[derive(Default)]
struct DistanceOptions<'a> {
    root_direct: Option<&'a HashSet<String>>,
    shared: Option<&'a HashSet<String>>,
    capture: bool,
}

#[derive(Default)]
struct DistanceRequest<'a> {
    shared: Option<&'a HashSet<String>>,
    capture: bool,
}

fn hash_file_distance_results(
    from: &HashFileData,
    to: &HashFileData,
    dep_edges: &BTreeMap<String, Vec<String>>,
    module_impacted: Option<&BTreeSet<String>>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
    request: DistanceRequest<'_>,
) -> Result<DistanceResults> {
    from.ensure_same_root_mode(to)?;
    let root_changes = impacted_types(&from.hashes, &to.hashes);
    let mut forced_direct = HashSet::new();
    let mut changes = if let (Some(a), Some(b)) = (&from.configured_roots, &to.configured_roots) {
        let mut inner = impacted_types(&a.hashes, &b.hashes);
        for (root, state) in root_changes {
            if b.hashes.contains_key(&root) {
                if state == ImpactType::Direct {
                    forced_direct.insert(root);
                }
            } else {
                inner.insert(root, state);
            }
        }
        inner
    } else {
        root_changes
    };
    for label in module_impacted.into_iter().flatten() {
        if to.configured_roots.is_some() {
            if to
                .hashes
                .get(label)
                .is_some_and(|hash| from.hashes.get(label).is_some_and(|old| old.same_as(hash)))
            {
                forced_direct.insert(label.clone());
            }
        } else if to.hashes.contains_key(label) && !changes.contains_key(label) {
            changes.insert(label.clone(), ImpactType::Direct);
        }
    }
    distances_for_types(
        &from.hashes,
        &to.hashes,
        dep_edges,
        target_types,
        exclude_external,
        changes,
        DistanceOptions {
            root_direct: to.configured_roots.as_ref().map(|_| &forced_direct),
            shared: request.shared,
            capture: request.capture,
        },
    )
}

fn distances_for_types(
    from: &BTreeMap<String, TargetHash>,
    to: &BTreeMap<String, TargetHash>,
    dep_edges: &BTreeMap<String, Vec<String>>,
    target_types: Option<&HashSet<String>>,
    exclude_external: bool,
    impact_types: HashMap<String, ImpactType>,
    options: DistanceOptions<'_>,
) -> Result<DistanceResults> {
    let DistanceOptions {
        root_direct,
        shared,
        capture,
    } = options;
    let mut dependents = HashMap::<&str, Vec<&str>>::new();
    let mut roots = Vec::new();
    for (label, impact) in &impact_types {
        if *impact == ImpactType::Direct {
            roots.push(label.as_str());
            if shared.is_none() {
                continue;
            }
        }
        let mut has_impacted_dependency = false;
        for dependency in dep_edges
            .get(label)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|dependency| impact_types.contains_key(*dependency))
        {
            has_impacted_dependency = true;
            dependents.entry(dependency).or_default().push(label);
        }
        if *impact == ImpactType::Indirect && !has_impacted_dependency {
            eprintln!(
                "[Warn] {label} was indirectly impacted, but has no impacted dependencies in the dep-edges file. Falling back to distance 0 -- see https://github.com/Tinder/bazel-diff/issues/268"
            );
            roots.push(label.as_str());
        }
    }
    let target_distances = reverse_distances(&dependents, &roots, |_, _| Some(1));
    let package_distances = reverse_distances(&dependents, &roots, |dependency, dependent| {
        Some(usize::from(
            dependency.split(':').next().unwrap_or(dependency)
                != dependent.split(':').next().unwrap_or(dependent),
        ))
    });
    let shared_distances = shared.map(|origins| {
        let roots = origins
            .iter()
            .filter_map(|label| {
                impact_types
                    .get_key_value(label)
                    .map(|(label, _)| label.as_str())
            })
            .collect::<Vec<_>>();
        let concrete = |dependency: &str, dependent: &str| {
            !dependency.starts_with("//external:") && !dependent.starts_with("//external:")
        };
        let target = reverse_distances(&dependents, &roots, |dependency, dependent| {
            concrete(dependency, dependent).then_some(1)
        });
        let package = reverse_distances(&dependents, &roots, |dependency, dependent| {
            concrete(dependency, dependent).then(|| {
                usize::from(
                    dependency.split(':').next().unwrap_or(dependency)
                        != dependent.split(':').next().unwrap_or(dependent),
                )
            })
        });
        (target, package)
    });
    // An unreached label belongs to or depends on a rootless changed component. The graph
    // identifies no origin for it, so retain the conservative fallback of distance zero.
    let unresolved = impact_types
        .keys()
        .map(String::as_str)
        .filter(|label| !target_distances.contains_key(label));
    let mut unresolved_origins = capture.then(Vec::new);
    let (count, representative) =
        unresolved.fold((0_usize, None), |(count, representative), label| {
            if let Some(origins) = &mut unresolved_origins {
                origins.push(label);
            }
            (
                count + 1,
                Some(representative.map_or(label, |prior: &str| prior.min(label))),
            )
        });
    if let Some(representative) = representative {
        eprintln!(
            "[Warn] {count} indirectly impacted targets have no path to a direct change or a target with missing changed dependencies, including {representative}; falling back to distance 0"
        );
    }
    let trace = capture.then(|| DistanceTrace {
        changed: impact_types.keys().cloned().collect(),
        direct: roots
            .iter()
            .copied()
            .chain(unresolved_origins.into_iter().flatten())
            .map(str::to_owned)
            .collect(),
        shared_changed: shared_distances
            .as_ref()
            .into_iter()
            .flat_map(|(target, _)| target.keys())
            .map(|key| (*key).to_owned())
            .collect(),
        shared_direct: shared
            .into_iter()
            .flatten()
            .filter(|key| {
                shared_distances
                    .as_ref()
                    .is_some_and(|(target, _)| target.contains_key(key.as_str()))
            })
            .cloned()
            .collect(),
    });
    let impacted = impact_types
        .keys()
        .chain(
            root_direct
                .into_iter()
                .flatten()
                .filter(|label| !impact_types.contains_key(*label)),
        )
        .filter(|label| {
            root_direct.is_none_or(|forced| {
                forced.contains(*label)
                    || to
                        .get(*label)
                        .is_some_and(|hash| !from.get(*label).is_some_and(|old| old.same_as(hash)))
            })
        })
        .map(String::as_str)
        .filter(|label| !exclude_external || !label.starts_with("//external:"))
        .filter_map(|label| {
            let (target_distance, package_distance) =
                match (target_distances.get(label), package_distances.get(label)) {
                    _ if root_direct.is_some_and(|roots| roots.contains(label)) => (0, 0),
                    (Some(target), Some(package)) => (*target, *package),
                    (None, None) => (0, 0),
                    _ => unreachable!("both distance walks use the same graph and origins"),
                };
            match accepts(label, to, target_types) {
                Ok(true) => Some(Ok(ImpactedTargetWithDistance {
                    label: label.to_owned(),
                    target_distance,
                    package_distance,
                })),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    // Rank each label once and sort the pairs. A key closure would look the label up in both maps
    // and clone it on every comparison: O(n log n) lookups and allocations for an n-label result.
    // The labels are distinct (they are the memo's keys), so an unstable sort orders them exactly
    // as a stable one would.
    let mut ranked = impacted
        .into_iter()
        .map(|target| {
            let rank = kind_rank(to.get(&target.label).or_else(|| from.get(&target.label)));
            (rank, target)
        })
        .collect::<Vec<_>>();
    ranked.sort_unstable_by(|(left_rank, left), (right_rank, right)| {
        left_rank
            .cmp(right_rank)
            .then_with(|| left.label.cmp(&right.label))
    });
    let shared = shared_distances
        .map(|(target, package)| {
            ranked
                .iter()
                .filter_map(|(_, row)| {
                    Some(ImpactedTargetWithDistance {
                        label: row.label.clone(),
                        target_distance: *target.get(row.label.as_str())?,
                        package_distance: *package.get(row.label.as_str())?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(DistanceResults {
        targets: ranked.into_iter().map(|(_, target)| target).collect(),
        shared,
        trace,
    })
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
    fn configured_root_rosters_round_trip_even_when_empty() {
        let empty = HashFileData::from_slice(
            br#"{"hashes":{},"metadata":{"configuredRoots":{"version":2,"roots":{},"hashes":{}}}}"#,
        )
        .unwrap();
        let one = HashFileData::from_slice(br#"{"hashes":{"//:a":"Rule#ab~cd"},"metadata":{"configuredRoots":{"version":2,"roots":{"//:a":"//:a|ab"},"hashes":{"//:a|ab":"Rule#ab~cd"}}}}"#).unwrap();
        for data in [&empty, &one] {
            let encoded = serde_json::to_vec(&data.serialized(true, false)).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&encoded).unwrap(),
                data.to_value(true, false)
            );
            let decoded = HashFileData::from_slice(&encoded).unwrap();
            assert_eq!(
                decoded.configured_roots.unwrap().roots,
                data.configured_roots.as_ref().unwrap().roots
            );
            assert!(data
                .ensure_same_root_mode(&HashFileData::default())
                .is_err());
        }
        let edges = BTreeMap::new();
        assert_eq!(
            hash_file_distances(&empty, &one, &edges, None, None, false).unwrap(),
            vec![ImpactedTargetWithDistance {
                label: "//:a".to_owned(),
                target_distance: 0,
                package_distance: 0
            }]
        );
        assert!(hash_file_distances(&one, &empty, &edges, None, None, false)
            .unwrap()
            .is_empty());
        let mut child = empty.clone();
        let source_hash = TargetHash::parse("SourceFile#ab~ab").unwrap();
        child
            .configured_roots
            .as_mut()
            .unwrap()
            .hashes
            .insert("//:source".to_owned(), source_hash.clone());
        let mut source = child.clone();
        source.hashes.insert("//:source".to_owned(), source_hash);
        source
            .configured_roots
            .as_mut()
            .unwrap()
            .roots
            .insert("//:source".to_owned(), "//:source".to_owned());
        assert_eq!(
            hash_file_distances(&child, &source, &edges, None, None, false).unwrap(),
            vec![ImpactedTargetWithDistance {
                label: "//:source".to_owned(),
                target_distance: 0,
                package_distance: 0,
            }]
        );
        assert!(
            hash_file_distances(&source, &child, &edges, None, None, false)
                .unwrap()
                .is_empty()
        );
        let mut invalid = one.to_value(true, false);
        invalid["metadata"]["configuredRoots"]["version"] = serde_json::json!(1);
        assert!(HashFileData::from_slice(&serde_json::to_vec(&invalid).unwrap()).is_err());
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

        data.dependency_fingerprint = None;
        data.partial = Some(PartialRecipes {
            version: 1,
            seed_hash: "00".repeat(32),
            package_seeds: true,
            rules: BTreeMap::from([(
                "//a:a".to_owned(),
                crate::partial::RuleRecipe("11".repeat(32), vec!["//b:b".into()], vec![]),
            )]),
            ..Default::default()
        });
        let streamed = serde_json::to_value(data.serialized(true, false)).unwrap();
        assert_eq!(streamed, data.to_value(true, false));
        assert_eq!(streamed["metadata"]["partialGraph"]["version"], 1);
        let parsed = HashFileData::from_slice(streamed.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.partial, data.partial);
        assert_eq!(parsed.hashes, data.hashes);
        assert!(parsed.tree.is_none());

        data.tree = Some("f".repeat(40));
        let streamed = serde_json::to_value(data.serialized(true, false)).unwrap();
        assert_eq!(streamed, data.to_value(true, false));
        assert_eq!(streamed["metadata"]["tree"], "f".repeat(40));
        let parsed = HashFileData::from_slice(streamed.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.tree, data.tree);
        // The tree alone is metadata enough to leave the legacy bare shape.
        data.partial = None;
        let streamed = serde_json::to_value(data.serialized(false, false)).unwrap();
        assert_eq!(streamed["metadata"]["tree"], "f".repeat(40));
        assert_eq!(streamed, data.to_value(false, false));
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
    fn bulk_built_maps_match_insertion_for_unsorted_and_repeated_labels() {
        // The readers build their maps in one pass; out-of-order and repeated labels must still
        // land exactly as entry-by-entry inserts would place them: sorted, last value wins.
        for bytes in [
            br#"{"//b:b":"b~b","//a:a":"a~a","//b:b":"c~c"}"#.as_slice(),
            br#"{"hashes":{"//b:b":"b~b","//a:a":"a~a","//b:b":"c~c"}}"#.as_slice(),
        ] {
            let parsed = HashFileData::from_slice(bytes).unwrap();
            assert_eq!(
                parsed.hashes,
                BTreeMap::from([
                    ("//a:a".into(), target("", "a", "a")),
                    ("//b:b".into(), target("", "c", "c")),
                ])
            );
        }
        // Bare labels after a wrapper override it; a wrapper after bare labels replaces them.
        let after = HashFileData::from_slice(br#"{"hashes":{"//a:a":"a~a"},"//a:a":"b~b"}"#);
        assert_eq!(after.unwrap().hashes["//a:a"], target("", "b", "b"));
        let before =
            HashFileData::from_slice(br#"{"//z:z":"z~z","hashes":{"//a:a":"a~a"}}"#).unwrap();
        assert_eq!(before.hashes.keys().collect::<Vec<_>>(), ["//a:a"]);

        let edges =
            dep_edges_from_slice(br#"{"//b:b":["//a:a"],"//a:a":[],"//b:b":["//c:c"]}"#).unwrap();
        assert_eq!(
            edges,
            BTreeMap::from([
                ("//a:a".to_owned(), vec![]),
                ("//b:b".to_owned(), vec!["//c:c".to_owned()]),
            ])
        );
        assert!(dep_edges_from_slice(br#"{"//a:a":"//b:b"}"#).is_err());
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
        // A rootless component has no attributable origin and conservatively uses zero.
        assert_eq!((cycle.target_distance, cycle.package_distance), (0, 0));
    }

    #[test]
    fn cyclic_distances_follow_origins_in_any_order() {
        let dependents = HashMap::from([
            ("//a:leaf", vec!["//a:a"]),
            ("//a:a", vec!["//a:b"]),
            ("//a:b", vec!["//a:c"]),
            ("//a:c", vec!["//a:a", "//a:consumer"]),
            ("//other:direct", vec!["//a:consumer", "//third:only"]),
        ]);
        let target = HashMap::from([
            ("//a:leaf", 0),
            ("//a:a", 1),
            ("//a:b", 2),
            ("//a:c", 3),
            ("//a:consumer", 1),
            ("//other:direct", 0),
            ("//third:only", 1),
        ]);
        let package = HashMap::from([
            ("//a:leaf", 0),
            ("//a:a", 0),
            ("//a:b", 0),
            ("//a:c", 0),
            ("//a:consumer", 0),
            ("//other:direct", 0),
            ("//third:only", 1),
        ]);
        for roots in [
            ["//a:leaf", "//other:direct"],
            ["//other:direct", "//a:leaf"],
        ] {
            assert_eq!(
                reverse_distances(&dependents, &roots, |_, _| Some(1)),
                target
            );
            // The consumer's shortest hop path crosses a package; the longer cycle path does not.
            assert_eq!(
                reverse_distances(&dependents, &roots, |dependency, dependent| {
                    Some(usize::from(
                        dependency.split(':').next() != dependent.split(':').next(),
                    ))
                }),
                package
            );
        }
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
