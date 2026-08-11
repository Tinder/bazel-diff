use crate::bazel::{target_name, BazelOptions};
use crate::model::{impacted_targets, HashFileData};
use crate::proto::blaze_query::Target;
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
    impacted_with_module_query(from, to, bazel.is_some(), |expression| {
        bazel.unwrap().query(expression, false)
    })
}

fn impacted_with_module_query(
    from: &HashFileData,
    to: &HashFileData,
    can_query: bool,
    mut query: impl FnMut(&str) -> Result<Vec<Target>>,
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
    if !can_query {
        return Ok(to.hashes.keys().cloned().collect());
    }
    let canonical_repos = from
        .hashes
        .keys()
        .chain(to.hashes.keys())
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
    match query(&expression) {
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
            let workspace_targets = to
                .hashes
                .keys()
                .filter(|label| !label.starts_with("@@") && !label.starts_with("//external:"))
                .cloned()
                .collect::<Vec<_>>();
            if workspace_targets.is_empty() {
                impacted.extend(to.hashes.keys().cloned());
            } else {
                impacted.extend(workspace_targets);
            }
        }
    }
    Ok(impacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TargetHash;
    use crate::proto::blaze_query::{target, Rule, Target};

    fn hash(kind: &str, value: &str) -> TargetHash {
        TargetHash {
            kind: kind.to_owned(),
            hash: value.to_owned(),
            direct_hash: value.to_owned(),
            deps: Vec::new(),
        }
    }

    fn graph(version: &str) -> String {
        format!(
            r#"noise before JSON
            {{
              "key": "<root>",
              "name": "root",
              "version": "",
              "apparentName": "root",
              "dependencies": [
                {{
                  "key": "rules_go@{version}",
                  "name": "rules_go",
                  "version": "{version}",
                  "apparentName": "rules_go",
                  "dependencies": [
                    {{
                      "key": "gazelle@0.40.0",
                      "name": "gazelle",
                      "version": "0.40.0",
                      "apparentName": "gazelle"
                    }}
                  ]
                }}
              ]
            }}"#
        )
    }

    fn root_only_graph() -> &'static str {
        r#"{
          "key": "<root>",
          "name": "root",
          "version": "",
          "apparentName": "root",
          "dependencies": []
        }"#
    }

    fn rule_target(name: &str) -> Target {
        let mut target = Target::default();
        target.r#type = target::Discriminator::Rule;
        target.rule = buffa::MessageField::some(Rule {
            name: name.to_owned(),
            rule_class: "sh_library".to_owned(),
            ..Default::default()
        });
        target
    }

    #[test]
    fn canonical_base_excludes_extension_repos() {
        assert!(canonical_is_base_for_module("rules_go+0.50.1", "rules_go"));
        assert!(canonical_is_base_for_module("rules_go~0.50.1", "rules_go"));
        assert!(!canonical_is_base_for_module(
            "rules_go++extensions+toolchains",
            "rules_go"
        ));
        assert!(!canonical_is_base_for_module("rules_go", "rules_go"));
        assert!(!canonical_is_base_for_module("rules_go_0.50.1", "rules_go"));
        assert!(!canonical_is_base_for_module("gazelle+0.40.0", "rules_go"));
    }

    #[test]
    fn parses_nested_modules_and_ignores_invalid_nodes() {
        let modules = parse_modules(&graph("0.50.1"));
        assert_eq!(modules["rules_go@0.50.1"].name, "rules_go");
        assert_eq!(modules["rules_go@0.50.1"].version, "0.50.1");
        assert_eq!(modules["gazelle@0.40.0"].name, "gazelle");
        assert_eq!(modules["<root>"].name, "root");
        assert!(parse_modules("not json").is_empty());
        assert!(parse_modules(r#"{"dependencies":[1, null, []]}"#).is_empty());
    }

    #[test]
    fn detects_added_removed_and_upgraded_modules() {
        assert_eq!(
            changed_modules(&graph("0.49.0"), &graph("0.50.1")),
            BTreeSet::from(["rules_go".to_owned()])
        );
        assert_eq!(
            changed_modules(root_only_graph(), &graph("0.50.1")),
            BTreeSet::from(["gazelle".to_owned(), "rules_go".to_owned()])
        );
        assert_eq!(
            changed_modules(&graph("0.50.1"), root_only_graph()),
            BTreeSet::from(["gazelle".to_owned(), "rules_go".to_owned()])
        );
        assert!(changed_modules(&graph("0.50.1"), &graph("0.50.1")).is_empty());
        assert!(changed_modules("invalid", &graph("0.50.1")).is_empty());
    }

    #[test]
    fn module_changes_cover_conservative_and_noop_paths() {
        let unchanged = BTreeMap::from([("//app:lib".to_owned(), hash("Rule", "same"))]);
        let changed = BTreeMap::from([
            ("//app:lib".to_owned(), hash("Rule", "new")),
            (
                "@@rules_go+0.50.1//go:def.bzl".to_owned(),
                hash("SourceFile", "external"),
            ),
        ]);
        let base = HashFileData {
            hashes: unchanged.clone(),
            module_graph_json: Some(graph("0.49.0")),
            ..Default::default()
        };

        let same_graph = HashFileData {
            hashes: changed.clone(),
            module_graph_json: base.module_graph_json.clone(),
            ..Default::default()
        };
        assert_eq!(
            impacted_with_module_changes(&base, &same_graph, None).unwrap(),
            BTreeSet::from([
                "//app:lib".to_owned(),
                "@@rules_go+0.50.1//go:def.bzl".to_owned()
            ])
        );

        let missing_graph = HashFileData {
            hashes: changed.clone(),
            module_graph_json: None,
            ..Default::default()
        };
        assert_eq!(
            impacted_with_module_changes(&base, &missing_graph, None).unwrap(),
            BTreeSet::from([
                "//app:lib".to_owned(),
                "@@rules_go+0.50.1//go:def.bzl".to_owned()
            ])
        );

        let upgraded = HashFileData {
            hashes: changed.clone(),
            module_graph_json: Some(graph("0.50.1")),
            ..Default::default()
        };
        assert_eq!(
            impacted_with_module_changes(&base, &upgraded, None).unwrap(),
            changed.keys().cloned().collect()
        );

        let extension_only = HashFileData {
            hashes: BTreeMap::from([(
                "@@rules_go++extensions+toolchain//:repo".to_owned(),
                hash("Rule", "changed"),
            )]),
            module_graph_json: Some(graph("0.50.1")),
            ..Default::default()
        };
        assert_eq!(
            impacted_with_module_changes(&base, &extension_only, None).unwrap(),
            BTreeSet::from(["@@rules_go++extensions+toolchain//:repo".to_owned()])
        );
    }

    #[test]
    fn module_query_unions_workspace_dependents() {
        let from = HashFileData {
            hashes: BTreeMap::from([
                (
                    "@@rules_go+0.49.0//go:def.bzl".into(),
                    hash("SourceFile", "old"),
                ),
                ("//app:unchanged".into(), hash("Rule", "same")),
            ]),
            module_graph_json: Some(graph("0.49.0")),
            ..Default::default()
        };
        let to = HashFileData {
            hashes: BTreeMap::from([
                (
                    "@@rules_go+0.50.1//go:def.bzl".into(),
                    hash("SourceFile", "new"),
                ),
                ("//app:unchanged".into(), hash("Rule", "same")),
            ]),
            module_graph_json: Some(graph("0.50.1")),
            ..Default::default()
        };
        let mut expression = String::new();
        let impacted = impacted_with_module_query(&from, &to, true, |query| {
            expression = query.to_owned();
            Ok(vec![rule_target("//app:one"), rule_target("//app:two")])
        })
        .unwrap();
        assert_eq!(
            expression,
            "rdeps(//..., @@rules_go+0.49.0//... + @@rules_go+0.50.1//...)"
        );
        assert!(impacted.contains("//app:one"));
        assert!(impacted.contains("//app:two"));
        assert!(!impacted.contains("//app:unchanged"));
    }

    #[test]
    fn removed_module_resolves_from_starting_hashes() {
        let from = HashFileData {
            hashes: BTreeMap::from([(
                "@@rules_go+0.49.0//go:def.bzl".into(),
                hash("SourceFile", "old"),
            )]),
            module_graph_json: Some(graph("0.49.0")),
            ..Default::default()
        };
        let to = HashFileData {
            hashes: BTreeMap::from([("//app:consumer".into(), hash("Rule", "same"))]),
            module_graph_json: Some(root_only_graph().to_owned()),
            ..Default::default()
        };
        assert!(impacted_with_module_query(&from, &to, true, |_| {
            Ok(vec![rule_target("//app:consumer")])
        })
        .unwrap()
        .contains("//app:consumer"));
    }

    #[test]
    fn module_query_failure_uses_workspace_or_all_hashes() {
        let from = HashFileData {
            hashes: BTreeMap::from([(
                "@@rules_go+0.49.0//go:def.bzl".into(),
                hash("SourceFile", "old"),
            )]),
            module_graph_json: Some(graph("0.49.0")),
            ..Default::default()
        };
        let mixed = HashFileData {
            hashes: BTreeMap::from([
                ("//app:app".into(), hash("Rule", "same")),
                (
                    "@@rules_go+0.50.1//go:def.bzl".into(),
                    hash("SourceFile", "new"),
                ),
                ("//external:rules_go".into(), hash("Rule", "bridge")),
            ]),
            module_graph_json: Some(graph("0.50.1")),
            ..Default::default()
        };
        assert_eq!(
            impacted_with_module_query(&from, &mixed, true, |_| {
                Err(anyhow::anyhow!("query failed"))
            })
            .unwrap(),
            BTreeSet::from([
                "//app:app".to_owned(),
                "@@rules_go+0.50.1//go:def.bzl".to_owned(),
                "//external:rules_go".to_owned()
            ])
        );

        let bzlmod_only = HashFileData {
            hashes: BTreeMap::from([
                (
                    "@@rules_go+0.50.1//go:def.bzl".into(),
                    hash("SourceFile", "new"),
                ),
                (
                    "@@rules_go+0.50.1//go:toolchain".into(),
                    hash("Rule", "toolchain"),
                ),
                ("//external:rules_go".into(), hash("Rule", "bridge")),
            ]),
            module_graph_json: Some(graph("0.50.1")),
            ..Default::default()
        };
        assert_eq!(
            impacted_with_module_query(&from, &bzlmod_only, true, |_| {
                Err(anyhow::anyhow!("query failed"))
            })
            .unwrap(),
            bzlmod_only.hashes.keys().cloned().collect()
        );
    }

    #[test]
    fn changed_module_does_not_match_extension_or_substring_repositories() {
        let from = HashFileData {
            hashes: BTreeMap::from([(
                "@@aspect_bazel_lib+1.0.0//:base".into(),
                hash("Rule", "old"),
            )]),
            module_graph_json: Some(
                graph("1.0.0")
                    .replace("rules_go", "aspect_bazel_lib")
                    .replace("gazelle", "unrelated"),
            ),
            ..Default::default()
        };
        let to = HashFileData {
            hashes: BTreeMap::from([
                (
                    "@@aspect_bazel_lib++toolchains+bats//:repo".into(),
                    hash("Rule", "same"),
                ),
                (
                    "@@my_aspect_bazel_lib_wrapper+2.0//:repo".into(),
                    hash("Rule", "same"),
                ),
            ]),
            module_graph_json: Some(
                graph("2.0.0")
                    .replace("rules_go", "aspect_bazel_lib")
                    .replace("gazelle", "unrelated"),
            ),
            ..Default::default()
        };
        assert_eq!(
            impacted_with_module_changes(&from, &to, None).unwrap(),
            to.hashes.keys().cloned().collect()
        );
    }
}
