//! Provenance for inferred repository memberships and their endpoint-local causes.
use crate::model::{Digest, HashFileData};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryMemberships {
    pub version: u32,
    pub groups: Vec<Vec<RepositoryMember>>,
    pub receivers: BTreeMap<String, RepositoryReceiver>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unresolved: BTreeSet<String>,
}

impl Default for RepositoryMemberships {
    fn default() -> Self {
        Self {
            version: 2,
            groups: Vec::new(),
            receivers: BTreeMap::new(),
            unresolved: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryMember {
    pub label: String,
    pub canonical_repository: Option<String>,
    pub selected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryReceiver {
    pub group: usize,
    pub ordinary_overlaps: BTreeSet<String>,
    pub direct_without_memberships: Option<String>,
}

impl RepositoryMemberships {
    pub fn contains(&self, receiver: &str, input: &str) -> bool {
        self.receivers.get(receiver).is_some_and(|receiver| {
            !receiver.ordinary_overlaps.contains(input)
                && self.groups.get(receiver.group).is_some_and(|members| {
                    members
                        .binary_search_by(|member| member.label.as_str().cmp(input))
                        .ok()
                        .is_some_and(|index| members[index].selected)
                })
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 2 {
            bail!("unsupported repository membership version {}", self.version);
        }
        for members in &self.groups {
            if members.is_empty()
                || members
                    .windows(2)
                    .any(|pair| pair[0].label >= pair[1].label)
                || members.iter().any(|member| {
                    !member.selected
                        || !member.label.starts_with("//external:")
                        || !member
                            .canonical_repository
                            .as_ref()
                            .is_some_and(|repository| {
                                repository.strip_prefix("@@").is_some_and(|name| {
                                    !name.is_empty()
                                        && !name.contains("//")
                                        && !name.chars().any(char::is_whitespace)
                                })
                            })
                })
            {
                bail!("invalid repository membership group");
            }
        }
        for (label, receiver) in &self.receivers {
            if !label.starts_with("//external:")
                || receiver.group >= self.groups.len()
                || receiver
                    .ordinary_overlaps
                    .iter()
                    .any(|label| !label.starts_with("//external:"))
                || receiver
                    .direct_without_memberships
                    .as_ref()
                    .is_some_and(|digest| !matches!(Digest::parse(digest), Digest::Sha256(_)))
            {
                bail!("invalid repository membership receiver {label}");
            }
        }
        Ok(())
    }

    fn members(&self, receiver: &RepositoryReceiver) -> BTreeSet<RepositoryMember> {
        self.groups[receiver.group]
            .iter()
            .filter(|member| member.selected && !receiver.ordinary_overlaps.contains(&member.label))
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryOrigin {
    pub label: String,
    pub canonical_repository: String,
    pub direct_hash: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryOrigins {
    pub base: BTreeSet<RepositoryOrigin>,
    pub head: BTreeSet<RepositoryOrigin>,
    pub unresolved: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrdinaryInputDescriptor {
    pub starting_hashes_sha256: String,
    pub final_hashes_sha256: String,
    pub head_edges_sha256: String,
}

struct Endpoint<'a> {
    name: &'static str,
    file: &'a HashFileData,
    memberships: &'a RepositoryMemberships,
    identities: HashMap<&'a str, Option<&'a str>>,
}

impl<'a> Endpoint<'a> {
    fn new(name: &'static str, file: &'a HashFileData) -> Result<Self> {
        let memberships = file
            .repository_memberships
            .as_ref()
            .with_context(|| format!("{name} repository membership provenance is unavailable"))?;
        memberships.validate()?;
        let mut identities = HashMap::new();
        for member in memberships.groups.iter().flatten() {
            let canonical = member.canonical_repository.as_deref();
            identities
                .entry(member.label.as_str())
                .and_modify(|prior| {
                    if *prior != canonical {
                        *prior = None;
                    }
                })
                .or_insert(canonical);
        }
        Ok(Self {
            name,
            file,
            memberships,
            identities,
        })
    }

    fn canonical_repository(&self, label: &str) -> Option<String> {
        let normalized = crate::bazel::main_repo_label_key(label);
        if normalized.starts_with("//external:") {
            return self
                .identities
                .get(normalized)
                .copied()
                .flatten()
                .map(str::to_owned);
        }
        if let Some(rest) = label.strip_prefix("@@") {
            let (repository, _) = rest.split_once("//")?;
            return (!repository.is_empty()).then(|| format!("@@{repository}"));
        }
        let (repository, _) = label.strip_prefix('@')?.split_once("//")?;
        self.identities
            .get(format!("//external:{repository}").as_str())
            .copied()
            .flatten()
            .map(str::to_owned)
    }

    fn record(
        &self,
        label: &str,
        origins: &mut BTreeSet<RepositoryOrigin>,
        unresolved: &mut BTreeSet<String>,
    ) {
        let Some(hash) = self.file.hashes.get(label) else {
            unresolved.insert(format!("{} origin {label} has no target hash", self.name));
            return;
        };
        let direct_hash = match self.memberships.receivers.get(label) {
            Some(receiver) => receiver.direct_without_memberships.clone(),
            None => Some(hash.direct_hash.to_string()),
        };
        let Some(direct_hash) = direct_hash else {
            unresolved.insert(format!(
                "{} origin {label} has no intrinsic digest",
                self.name
            ));
            return;
        };
        let Some(canonical_repository) = self.canonical_repository(label) else {
            unresolved.insert(format!(
                "{} origin {label} has no canonical repository",
                self.name
            ));
            return;
        };
        origins.insert(RepositoryOrigin {
            label: label.to_owned(),
            canonical_repository,
            direct_hash,
        });
    }
}

struct ReceiverDelta {
    base_only: BTreeSet<RepositoryMember>,
    head_only: BTreeSet<RepositoryMember>,
    intrinsic: Option<(String, String)>,
    paired: bool,
}

impl ReceiverDelta {
    fn roster_changed(&self) -> bool {
        !self.paired || !self.base_only.is_empty() || !self.head_only.is_empty()
    }
}

/// Extract selected-membership causes for a batch of roots from one parsed ordinary pair.
/// The caller supplies roots from the provisional path; this output alone never clears a root.
pub fn extract_origins(
    from: &HashFileData,
    to: &HashFileData,
    edges: &BTreeMap<String, Vec<String>>,
    changed: &BTreeSet<String>,
    direct: &BTreeSet<String>,
    roots: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, RepositoryOrigins>> {
    if !direct.is_subset(changed) {
        bail!("repository origin lies outside the changed graph");
    }
    let base = Endpoint::new("base", from)?;
    let head = Endpoint::new("head", to)?;
    let receiver_names: BTreeSet<_> = base
        .memberships
        .receivers
        .keys()
        .chain(head.memberships.receivers.keys())
        .collect();
    let mut receivers = BTreeMap::new();
    for label in receiver_names {
        let before = base.memberships.receivers.get(label);
        let after = head.memberships.receivers.get(label);
        let old = before
            .map(|receiver| base.memberships.members(receiver))
            .unwrap_or_default();
        let new = after
            .map(|receiver| head.memberships.members(receiver))
            .unwrap_or_default();
        receivers.insert(
            label.as_str(),
            ReceiverDelta {
                base_only: old.difference(&new).cloned().collect(),
                head_only: new.difference(&old).cloned().collect(),
                intrinsic: before
                    .and_then(|receiver| receiver.direct_without_memberships.clone())
                    .zip(after.and_then(|receiver| receiver.direct_without_memberships.clone())),
                paired: before.is_some() && after.is_some(),
            },
        );
    }
    let mut results = BTreeMap::new();
    for (root, key) in roots {
        let mut result = RepositoryOrigins::default();
        if !changed.contains(key) {
            result.unresolved.insert(format!(
                "root {root} is outside the provisional changed graph"
            ));
            results.insert(root.clone(), result);
            continue;
        }
        let mut queue = VecDeque::from([(key.as_str(), false)]);
        let mut visited = HashSet::new();
        while let Some((label, crossed_selected)) = queue.pop_front() {
            if !visited.insert((label, crossed_selected)) {
                continue;
            }
            if direct.contains(label) {
                if let Some(delta) = receivers
                    .get(label)
                    .filter(|delta| crossed_selected || delta.roster_changed())
                {
                    if !delta.paired {
                        result.unresolved.insert(format!(
                            "repository receiver {label} is absent from one endpoint"
                        ));
                    } else {
                        for member in &delta.base_only {
                            base.record(&member.label, &mut result.base, &mut result.unresolved);
                        }
                        for member in &delta.head_only {
                            head.record(&member.label, &mut result.head, &mut result.unresolved);
                        }
                        match &delta.intrinsic {
                            Some((before, after)) if before != after => {
                                base.record(label, &mut result.base, &mut result.unresolved);
                                head.record(label, &mut result.head, &mut result.unresolved);
                            }
                            Some(_) if !delta.roster_changed() => {
                                result.unresolved.insert(format!(
                                    "repository receiver {label} has an unattributed direct change"
                                ));
                            }
                            Some(_) => {}
                            None => {
                                result.unresolved.insert(format!(
                                    "repository receiver {label} has no paired intrinsic digests"
                                ));
                            }
                        }
                    }
                } else if crossed_selected {
                    let before = from.hashes.get(label);
                    let after = to.hashes.get(label);
                    if after.is_some_and(|after| {
                        before.is_none_or(|before| before.direct_hash != after.direct_hash)
                    }) {
                        if before.is_some() {
                            base.record(label, &mut result.base, &mut result.unresolved);
                        }
                        head.record(label, &mut result.head, &mut result.unresolved);
                    } else {
                        result.unresolved.insert(format!(
                            "changed row {label} has no attributable direct change"
                        ));
                    }
                }
            }
            let mut has_changed_child = false;
            for child in edges.get(label).into_iter().flatten() {
                if changed.contains(child) {
                    has_changed_child = true;
                    queue.push_back((
                        child,
                        crossed_selected || head.memberships.contains(label, child),
                    ));
                }
            }
            if crossed_selected && !direct.contains(label) && !has_changed_child {
                result.unresolved.insert(format!(
                    "changed path ends before a direct origin at {label}"
                ));
            }
        }
        if result.base.is_empty() && result.head.is_empty() && result.unresolved.is_empty() {
            result
                .unresolved
                .insert(format!("root {root} has no attributable repository origin"));
        }
        results.insert(root.clone(), result);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TargetHash;

    fn digest(value: u8) -> String {
        format!("{value:064x}")
    }

    fn hash(overall: u8, direct: u8) -> TargetHash {
        TargetHash::parse(&format!("Rule#{}~{}", digest(overall), digest(direct))).unwrap()
    }

    fn member(name: &str) -> RepositoryMember {
        RepositoryMember {
            label: format!("//external:{name}"),
            canonical_repository: Some(format!("@@ext++values+{name}")),
            selected: true,
        }
    }

    fn endpoint(side: u8, leaf: &str, hub_intrinsic: u8) -> HashFileData {
        let hub = member("hub");
        let leaf = member(leaf);
        let mut owner_members = vec![hub.clone(), leaf.clone()];
        owner_members.sort();
        HashFileData {
            hashes: BTreeMap::from([
                ("//:root".into(), hash(side, 0)),
                ("//external:owner".into(), hash(side, side)),
                (hub.label.clone(), hash(side, side)),
                (leaf.label.clone(), hash(side + 10, side + 10)),
            ]),
            repository_memberships: Some(RepositoryMemberships {
                groups: vec![owner_members, vec![leaf]],
                receivers: BTreeMap::from([
                    (
                        "//external:owner".into(),
                        RepositoryReceiver {
                            group: 0,
                            ordinary_overlaps: BTreeSet::new(),
                            direct_without_memberships: Some(digest(1)),
                        },
                    ),
                    (
                        hub.label,
                        RepositoryReceiver {
                            group: 1,
                            ordinary_overlaps: BTreeSet::new(),
                            direct_without_memberships: Some(digest(hub_intrinsic)),
                        },
                    ),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn extract(before: &HashFileData, after: &HashFileData, leaf: &str) -> RepositoryOrigins {
        let leaf = format!("//external:{leaf}");
        let edges = BTreeMap::from([
            ("//:root".into(), vec!["//external:owner".into()]),
            (
                "//external:owner".into(),
                vec!["//external:hub".into(), leaf.clone()],
            ),
            ("//external:hub".into(), vec![leaf.clone()]),
            (leaf.clone(), Vec::new()),
        ]);
        let changed = edges.keys().cloned().collect();
        let direct = BTreeSet::from(["//external:owner".into(), "//external:hub".into(), leaf]);
        extract_origins(
            before,
            after,
            &edges,
            &changed,
            &direct,
            &BTreeMap::from([("//:root".into(), "//:root".into())]),
        )
        .unwrap()
        .remove("//:root")
        .unwrap()
    }

    #[test]
    fn roster_only_receivers_reduce_to_endpoint_members() {
        let before = endpoint(2, "old", 3);
        let after = endpoint(4, "new", 3);
        let origins = extract(&before, &after, "new");
        assert!(origins.unresolved.is_empty(), "{origins:?}");
        assert_eq!(
            origins
                .base
                .iter()
                .map(|origin| origin.label.as_str())
                .collect::<Vec<_>>(),
            ["//external:old"]
        );
        assert_eq!(
            origins
                .head
                .iter()
                .map(|origin| origin.label.as_str())
                .collect::<Vec<_>>(),
            ["//external:new"]
        );
        assert_eq!(origins.base.first().unwrap().direct_hash, digest(12));
        assert_eq!(origins.head.first().unwrap().direct_hash, digest(14));

        let mut no_roster = before.clone();
        no_roster
            .repository_memberships
            .as_mut()
            .unwrap()
            .unresolved
            .insert("evaluated MODULE registrations are unavailable".into());
        assert!(extract(&no_roster, &after, "new").unresolved.is_empty());

        let changed_hub = endpoint(4, "new", 5);
        let origins = extract(&before, &changed_hub, "new");
        assert!(origins.unresolved.is_empty(), "{origins:?}");
        assert!(origins.base.iter().any(|origin| {
            origin.label == "//external:hub"
                && origin.canonical_repository == "@@ext++values+hub"
                && origin.direct_hash == digest(3)
        }));
        assert!(origins
            .head
            .iter()
            .any(|origin| { origin.label == "//external:hub" && origin.direct_hash == digest(5) }));
    }

    #[test]
    fn missing_receiver_or_intrinsic_identity_stays_unresolved() {
        let before = endpoint(2, "old", 3);
        let mut after = endpoint(4, "new", 3);
        after
            .repository_memberships
            .as_mut()
            .unwrap()
            .receivers
            .get_mut("//external:owner")
            .unwrap()
            .direct_without_memberships = Some(digest(9));
        let origins = extract(&before, &after, "new");
        assert!(origins
            .unresolved
            .iter()
            .any(|reason| reason.contains("origin //external:owner has no canonical repository")));

        let mut missing = before;
        missing
            .repository_memberships
            .as_mut()
            .unwrap()
            .receivers
            .remove("//external:owner");
        let origins = extract(&missing, &after, "new");
        assert!(origins
            .unresolved
            .contains("repository receiver //external:owner is absent from one endpoint"));
    }

    #[test]
    fn a_real_source_after_membership_is_an_endpoint_origin() {
        let mut before = endpoint(2, "payload", 3);
        let mut after = endpoint(4, "payload", 3);
        let source = "@@ext++values+payload//:value.txt";
        before.hashes.insert(source.into(), hash(20, 20));
        after.hashes.insert(source.into(), hash(21, 21));
        let edges = BTreeMap::from([
            ("//:root".into(), vec!["//external:hub".into()]),
            ("//external:hub".into(), vec!["//external:payload".into()]),
            ("//external:payload".into(), vec![source.into()]),
            (source.into(), Vec::new()),
        ]);
        let origins = extract_origins(
            &before,
            &after,
            &edges,
            &edges.keys().cloned().collect(),
            &BTreeSet::from([source.into()]),
            &BTreeMap::from([("//:root".into(), "//:root".into())]),
        )
        .unwrap()
        .remove("//:root")
        .unwrap();
        assert!(origins.unresolved.is_empty(), "{origins:?}");
        assert_eq!(origins.base.first().unwrap().direct_hash, digest(20));
        assert_eq!(origins.head.first().unwrap().direct_hash, digest(21));
        assert_eq!(
            origins.head.first().unwrap().canonical_repository,
            "@@ext++values+payload"
        );
    }

    #[test]
    fn a_genuine_overlap_is_never_a_selected_membership() {
        let mut file = endpoint(2, "payload", 3);
        let memberships = file.repository_memberships.as_mut().unwrap();
        memberships
            .receivers
            .get_mut("//external:hub")
            .unwrap()
            .ordinary_overlaps
            .insert("//external:payload".into());
        assert!(!memberships.contains("//external:hub", "//external:payload"));
        assert!(memberships.contains("//external:owner", "//external:payload"));
    }
}
