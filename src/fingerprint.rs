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
        .and_then(|output| parse_bazel_version(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn parse_bazel_version(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| {
            line.strip_prefix("Build label: ")
                .or_else(|| line.strip_prefix("bazel "))
        })
        .map(|value| value.trim().to_owned())
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

    #[test]
    fn fingerprint_changes_for_each_component() {
        let base = FingerprintInputs {
            bazel_diff_version: "1".into(),
            bazel_version: "2".into(),
            module_lock_content: None,
            bazelrc_contents: BTreeMap::new(),
            flags: BTreeMap::new(),
        };
        let baseline = compute(&base);
        assert_eq!(baseline.components.len(), 5);
        for changed in [
            FingerprintInputs {
                bazel_diff_version: "changed".into(),
                ..base.clone()
            },
            FingerprintInputs {
                bazel_version: "changed".into(),
                ..base.clone()
            },
            FingerprintInputs {
                module_lock_content: Some(b"lock".to_vec()),
                ..base.clone()
            },
            FingerprintInputs {
                bazelrc_contents: BTreeMap::from([(".bazelrc".into(), b"build --x".to_vec())]),
                ..base.clone()
            },
            FingerprintInputs {
                flags: BTreeMap::from([("x".into(), "y".into())]),
                ..base.clone()
            },
        ] {
            assert_ne!(compute(&changed).fingerprint, baseline.fingerprint);
        }
    }

    #[test]
    fn reads_bazelrc_imports_and_skips_missing_files() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("config")).unwrap();
        fs::write(
            workspace.path().join(".bazelrc"),
            "build --color=yes\nimport config/common.bazelrc\ntry-import %workspace%/missing\ntry-import %workspace%/config/optional.bazelrc\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("config/common.bazelrc"),
            "common --announce_rc\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("config/optional.bazelrc"),
            "test --test_output=errors\n",
        )
        .unwrap();

        let files = read_bazelrcs(workspace.path());
        assert_eq!(files.len(), 3);
        assert!(files.contains_key(".bazelrc"));
        assert!(files.contains_key("config/common.bazelrc"));
        assert!(files.contains_key("config/optional.bazelrc"));
        assert!(read_bazelrcs(&workspace.path().join("absent")).is_empty());
    }

    #[test]
    fn gathers_workspace_inputs_when_bazel_is_unavailable() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("MODULE.bazel.lock"), "lock").unwrap();
        fs::write(workspace.path().join(".bazelrc"), "build --x\n").unwrap();
        let flags = BTreeMap::from([("useCquery".into(), "true".into())]);

        let inputs = gather(
            workspace.path(),
            Path::new("/definitely/missing/bazel"),
            flags.clone(),
        );
        assert_eq!(inputs.bazel_version, "unknown");
        assert_eq!(inputs.module_lock_content, Some(b"lock".to_vec()));
        assert_eq!(inputs.flags, flags);
        assert!(inputs.bazelrc_contents.contains_key(".bazelrc"));
    }

    #[test]
    fn parses_supported_bazel_version_output() {
        assert_eq!(
            parse_bazel_version("Build label: 9.1.2\n"),
            Some("9.1.2".to_owned())
        );
        assert_eq!(
            parse_bazel_version("bazel 8.5.1\n"),
            Some("8.5.1".to_owned())
        );
        assert_eq!(parse_bazel_version("unexpected"), None);
    }

    #[test]
    fn writes_pretty_json_to_a_file() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let flags = BTreeMap::from([("x".into(), "y".into())]);
        let result = FingerprintResult {
            fingerprint: "abc".into(),
            components: BTreeMap::from([("one".into(), "two".into())]),
        };
        write_json(Some(output.path()), &result, &flags).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(output.path()).unwrap()).unwrap();
        assert_eq!(value["fingerprint"], "abc");
        assert_eq!(value["components"]["one"], "two");
        assert_eq!(value["flags"]["x"], "y");
        assert!(write_json(
            Some(Path::new("/definitely/missing/parent/output.json")),
            &result,
            &flags,
        )
        .is_err());
    }
}
