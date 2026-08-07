use crate::bazel::{target_name, BazelOptions};
use crate::model::{impacted_targets, HashFileData};
use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Module {
    name: String,
    version: String,
}

fn parse_modules(json: &str) -> BTreeMap<String, Module> {
    fn walk(value: &Value, modules: &mut BTreeMap<String, Module>) {
        let Some(object) = value.as_object() else {
            return;
        };
        if let (Some(key), Some(name), Some(version), Some(_apparent)) = (
            object.get("key").and_then(Value::as_str),
            object.get("name").and_then(Value::as_str),
            object.get("version").and_then(Value::as_str),
            object.get("apparentName").and_then(Value::as_str),
        ) {
            if !name.is_empty() {
                modules.insert(
                    key.to_owned(),
                    Module {
                        name: name.to_owned(),
                        version: version.to_owned(),
                    },
                );
            }
        }
        if let Some(dependencies) = object.get("dependencies").and_then(Value::as_array) {
            for dependency in dependencies {
                walk(dependency, modules);
            }
        }
    }

    let start = json.find('{').unwrap_or(json.len());
    let Ok(value) = serde_json::from_str::<Value>(&json[start..]) else {
        return BTreeMap::new();
    };
    let mut modules = BTreeMap::new();
    walk(&value, &mut modules);
    modules
}

fn changed_modules(from: &str, to: &str) -> BTreeSet<String> {
    let from = parse_modules(from);
    let to = parse_modules(to);
    if from.is_empty() != to.is_empty() {
        eprintln!(
            "[Warn] Module graph parse asymmetry detected. Falling back to per-target hash diff. See https://github.com/Tinder/bazel-diff/issues/335"
        );
        return BTreeSet::new();
    }
    from.keys()
        .chain(to.keys())
        .filter_map(|key| match (from.get(key), to.get(key)) {
            (Some(left), Some(right)) if left.version == right.version => None,
            (_, Some(module)) | (Some(module), None) => Some(module.name.clone()),
            (None, None) => None,
        })
        .collect()
}

fn canonical_is_base_for_module(canonical: &str, module: &str) -> bool {
    let Some(rest) = canonical.strip_prefix(module) else {
        return false;
    };
    let Some(separator) = rest.chars().next() else {
        return false;
    };
    if separator != '+' && separator != '~' {
        return false;
    }
    let version = &rest[separator.len_utf8()..];
    !version.contains('+') && !version.contains('~')
}

pub fn impacted_with_module_changes(
    from: &HashFileData,
    to: &HashFileData,
    bazel: Option<&BazelOptions>,
) -> Result<BTreeSet<String>> {
    let mut impacted = impacted_targets(&from.hashes, &to.hashes, None, false)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    if from.module_graph_json == to.module_graph_json {
        return Ok(impacted);
    }
    let (Some(from_graph), Some(to_graph)) = (
        from.module_graph_json.as_deref(),
        to.module_graph_json.as_deref(),
    ) else {
        return Ok(impacted);
    };
    let changed = changed_modules(from_graph, to_graph);
    if changed.is_empty() {
        return Ok(impacted);
    }
    let Some(bazel) = bazel else {
        return Ok(to.hashes.keys().cloned().collect());
    };
    let canonical_repos = to
        .hashes
        .keys()
        .filter_map(|label| {
            label
                .strip_prefix("@@")
                .and_then(|label| label.split_once("//"))
                .map(|(repo, _)| repo)
        })
        .collect::<HashSet<_>>();
    let repos = canonical_repos
        .into_iter()
        .filter(|repo| {
            changed
                .iter()
                .any(|module| canonical_is_base_for_module(repo, module))
        })
        .collect::<BTreeSet<_>>();
    if repos.is_empty() {
        return Ok(impacted);
    }
    let expression = format!(
        "rdeps(//..., {})",
        repos
            .iter()
            .map(|repo| format!("@@{repo}//..."))
            .collect::<Vec<_>>()
            .join(" + ")
    );
    match bazel.query(&expression, false) {
        Ok(targets) => {
            impacted.extend(
                targets
                    .iter()
                    .filter_map(target_name)
                    .filter(|label| !label.starts_with("@@"))
                    .map(str::to_owned),
            );
        }
        Err(error) => {
            eprintln!(
                "[Warn] Unioned rdeps query failed ({error}); conservatively including workspace targets"
            );
            impacted.extend(
                to.hashes
                    .keys()
                    .filter(|label| !label.starts_with("@@") && !label.starts_with("//external:"))
                    .cloned(),
            );
        }
    }
    Ok(impacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_base_excludes_extension_repos() {
        assert!(canonical_is_base_for_module("rules_go+0.50.1", "rules_go"));
        assert!(canonical_is_base_for_module("rules_go~0.50.1", "rules_go"));
        assert!(!canonical_is_base_for_module(
            "rules_go++extensions+toolchains",
            "rules_go"
        ));
    }
}
