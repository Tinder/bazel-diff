use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct FingerprintInputs {
    pub bazel_diff_version: String,
    pub bazel_version: String,
    pub module_lock_content: Option<Vec<u8>>,
    pub bazelrc_contents: BTreeMap<String, Vec<u8>>,
    pub flags: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FingerprintResult {
    pub fingerprint: String,
    pub components: BTreeMap<String, String>,
}

fn digest(parts: impl IntoIterator<Item = Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hex::encode(hasher.finalize())
}

pub fn compute(inputs: &FingerprintInputs) -> FingerprintResult {
    let mut components = BTreeMap::new();
    components.insert(
        "bazelDiffVersion".to_owned(),
        digest([inputs.bazel_diff_version.as_bytes().to_vec()]),
    );
    components.insert(
        "bazelVersion".to_owned(),
        digest([inputs.bazel_version.as_bytes().to_vec()]),
    );
    components.insert(
        "moduleLock".to_owned(),
        digest(match &inputs.module_lock_content {
            Some(content) => vec![b"present".to_vec(), content.clone()],
            None => vec![b"absent".to_vec()],
        }),
    );
    components.insert(
        "bazelrc".to_owned(),
        digest(
            inputs
                .bazelrc_contents
                .iter()
                .flat_map(|(path, content)| [path.as_bytes().to_vec(), content.clone()]),
        ),
    );
    components.insert(
        "flags".to_owned(),
        digest(
            inputs
                .flags
                .iter()
                .flat_map(|(key, value)| [key.as_bytes().to_vec(), value.as_bytes().to_vec()]),
        ),
    );
    let fingerprint = digest(
        components
            .iter()
            .flat_map(|(key, value)| [key.as_bytes().to_vec(), value.as_bytes().to_vec()]),
    );
    FingerprintResult {
        fingerprint,
        components,
    }
}

pub fn gather(
    workspace: &Path,
    bazel: &Path,
    flags: BTreeMap<String, String>,
) -> FingerprintInputs {
    FingerprintInputs {
        bazel_diff_version: env!("CARGO_PKG_VERSION").to_owned(),
        bazel_version: bazel_version(workspace, bazel),
        module_lock_content: fs::read(workspace.join("MODULE.bazel.lock")).ok(),
        bazelrc_contents: read_bazelrcs(workspace),
        flags,
    }
}

fn bazel_version(workspace: &Path, bazel: &Path) -> String {
    Command::new(bazel)
        .current_dir(workspace)
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .find_map(|line| {
                    line.strip_prefix("Build label: ")
                        .or_else(|| line.strip_prefix("bazel "))
                })
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn read_bazelrcs(workspace: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut result = BTreeMap::new();
    let root = workspace.join(".bazelrc");
    let Ok(bytes) = fs::read(&root) else {
        return result;
    };
    result.insert(".bazelrc".to_owned(), bytes.clone());
    for line in String::from_utf8_lossy(&bytes).lines() {
        let line = line.trim();
        if !line.starts_with("import ") && !line.starts_with("try-import ") {
            continue;
        }
        let raw = line.split_once(' ').map_or("", |(_, path)| path.trim());
        let resolved = raw.replace("%workspace%", &workspace.to_string_lossy());
        let path = PathBuf::from(resolved);
        let absolute = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        if let Ok(contents) = fs::read(&absolute) {
            let key = absolute
                .strip_prefix(workspace)
                .unwrap_or(&absolute)
                .to_string_lossy()
                .into_owned();
            result.insert(key, contents);
        }
    }
    result
}

pub fn write_json(
    path: Option<&Path>,
    result: &FingerprintResult,
    flags: &BTreeMap<String, String>,
) -> Result<()> {
    let value = serde_json::json!({
        "fingerprint": result.fingerprint,
        "components": result.components,
        "flags": flags,
    });
    let json = serde_json::to_string_pretty(&value)? + "\n";
    match path {
        Some(path) if path != Path::new("-") => {
            fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?
        }
        _ => print!("{json}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_map_order_independent() {
        let first = FingerprintInputs {
            bazel_diff_version: "1".into(),
            bazel_version: "2".into(),
            module_lock_content: None,
            bazelrc_contents: BTreeMap::from([
                ("b".into(), b"2".to_vec()),
                ("a".into(), b"1".to_vec()),
            ]),
            flags: BTreeMap::from([("z".into(), "2".into()), ("a".into(), "1".into())]),
        };
        let mut second = first.clone();
        second.bazelrc_contents = first.bazelrc_contents.clone().into_iter().rev().collect();
        second.flags = first.flags.clone().into_iter().rev().collect();
        assert_eq!(compute(&first).fingerprint, compute(&second).fingerprint);
    }
}
