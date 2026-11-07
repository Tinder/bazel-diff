use sha2::{Digest, Sha256};

type DigestBytes = [u8; 32];

pub(crate) enum Visit<T> {
    Complete(T),
    Pending(usize),
}

pub(crate) enum Completion<K, V> {
    Pending(usize),
    Single(K, V),
    Cycle(Vec<(K, V)>),
}

struct Entry<K, V> {
    key: K,
    lowlink: usize,
    value: Option<V>,
}

/// Tracks strongly connected components during a depth-first traversal. Positions remain
/// valid while pending: completed components leave only from the end of the stack.
pub(crate) struct Components<K, V> {
    entries: Vec<Entry<K, V>>,
}

impl<K, V> Default for Components<K, V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<K, V> Components<K, V> {
    pub(crate) fn enter(&mut self, key: K) -> usize {
        let position = self.entries.len();
        self.entries.push(Entry {
            key,
            lowlink: position,
            value: None,
        });
        position
    }

    pub(crate) fn reaches(&mut self, position: usize, dependency: usize) {
        self.entries[position].lowlink = self.entries[position].lowlink.min(dependency);
    }

    pub(crate) fn finish(&mut self, position: usize, value: V) -> Completion<K, V> {
        let entry = &mut self.entries[position];
        entry.value = Some(value);
        if entry.lowlink != position {
            return Completion::Pending(entry.lowlink);
        }
        if self.entries.len() == position + 1 {
            let entry = self.entries.pop().expect("the component root is pending");
            return Completion::Single(entry.key, entry.value.expect("the root is finished"));
        }
        Completion::Cycle(
            self.entries
                .drain(position..)
                .map(|entry| (entry.key, entry.value.expect("the component is finished")))
                .collect(),
        )
    }
}

/// Each value includes its member's direct contributions and completed dependencies outside
/// the component. Internal edge labels already contribute to the member's direct digest.
pub(crate) fn component_digest<'a>(
    members: impl Iterator<Item = (&'a str, DigestBytes)>,
) -> DigestBytes {
    let mut members = members.collect::<Vec<_>>();
    members.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(b"bazel-diff-component\0");
    for (label, digest) in members {
        hasher.update((label.len() as u64).to_le_bytes());
        hasher.update(label.as_bytes());
        hasher.update(digest);
    }
    hasher.finalize().into()
}

pub(crate) fn member_digest(
    component: DigestBytes,
    label: &str,
    outgoing: DigestBytes,
) -> DigestBytes {
    let mut hasher = Sha256::new();
    hasher.update(b"bazel-diff-component-member\0");
    hasher.update(component);
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update(outgoing);
    hasher.finalize().into()
}
