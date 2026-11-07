use crate::support::*;
use std::collections::BTreeSet;
use std::fs;

fn zipped_golden_case(first_zip: &str, second_zip: &str, generate_args: &[&str], golden: &str) {
    let first = extract_fixture(first_zip);
    let second = extract_fixture(second_zip);
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let result = output.path().join("impacted.txt");
    assert!(generate(first.path(), &from, generate_args)
        .status
        .success());
    assert!(generate(second.path(), &to, generate_args).status.success());
    assert!(impacted(second.path(), &from, &to, &result, &[])
        .status
        .success());
    assert_eq!(
        filter_internal(read_lines(&result)),
        filter_internal(golden_lines(golden))
    );
}

#[test]
fn fine_grained_workspace_external_repo() {
    zipped_golden_case(
        "fine-grained-hash-external-repo-test-1.zip",
        "fine-grained-hash-external-repo-test-2.zip",
        &["--fineGrainedHashExternalRepos", "@bazel_diff_maven"],
        "fine-grained-hash-external-repo-test-impacted-targets.txt",
    );
}

#[test]
fn fine_grained_bzlmod_repo() {
    zipped_golden_case(
        "fine-grained-hash-bzlmod-test-1.zip",
        "fine-grained-hash-bzlmod-test-2.zip",
        &["--fineGrainedHashExternalRepos", "@bazel_diff_maven"],
        "fine-grained-hash-bzlmod-test-impacted-targets.txt",
    );
}

#[test]
fn fine_grained_bzlmod_repo_cquery() {
    zipped_golden_case(
        "fine-grained-hash-bzlmod-test-1.zip",
        "fine-grained-hash-bzlmod-test-2.zip",
        // The apparent name, as the query-mode case passes: it names the same repository on
        // every Bazel version, where the canonical spelling changed from `~~maven~maven`
        // (Bazel 7) to `++maven+maven` (Bazel 8). With cquery's canonical labels now reaching
        // the `//external:bazel_diff_maven` node, a flag that names nothing makes the lock-file
        // change fan out to every maven consumer instead of the fine-grained set.
        &[
            "--fineGrainedHashExternalRepos",
            "@bazel_diff_maven",
            "--useCquery",
        ],
        "fine-grained-hash-bzlmod-cquery-test-impacted-targets.txt",
    );
}

#[test]
fn bzlmod_transitive_deps_query() {
    zipped_golden_case(
        "bzlmod-transitive-test-1.zip",
        "bzlmod-transitive-test-2.zip",
        &["--fineGrainedHashExternalRepos", "@bazel_diff_maven"],
        "bzlmod-transitive-test-impacted-targets.txt",
    );
}

#[test]
fn bzlmod_transitive_deps_cquery() {
    zipped_golden_case(
        "bzlmod-transitive-test-1.zip",
        "bzlmod-transitive-test-2.zip",
        // Apparent name for the same reason as fine_grained_bzlmod_repo_cquery.
        &[
            "--fineGrainedHashExternalRepos",
            "@bazel_diff_maven",
            "--useCquery",
        ],
        "bzlmod-transitive-test-cquery-impacted-targets.txt",
    );
}

#[test]
#[ignore = "fixture pins Bazel 7 layout that is incompatible with Bazel 8+ tools/windows packaging"]
fn bzlmod_cc_transitive_deps_query() {
    zipped_golden_case(
        "bzlmod-cc-transitive-test-1.zip",
        "bzlmod-cc-transitive-test-2.zip",
        &[],
        "bzlmod-cc-transitive-test-impacted-targets.txt",
    );
}

fn cquery_platform_case(second_zip: &str, platform: &str, golden: &str) {
    let first = extract_fixture("cquery-test-base.zip");
    let second = extract_fixture(second_zip);
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let result = output.path().join("impacted.txt");
    let platform_arg = format!("--platforms=//:{platform}");
    let args = [
        "--useCquery",
        "--cqueryCommandOptions",
        platform_arg.as_str(),
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
    ];
    assert!(generate(first.path(), &from, &args).status.success());
    assert!(generate(second.path(), &to, &args).status.success());
    assert!(impacted(second.path(), &from, &to, &result, &[])
        .status
        .success());
    assert_eq!(
        filter_internal(read_lines(&result)),
        filter_internal(golden_lines(golden))
    );
}

#[test]
fn cquery_external_dependency_change_platforms() {
    cquery_platform_case(
        "cquery-test-guava-upgrade.zip",
        "android",
        "cquery-test-guava-upgrade-android-impacted-targets.txt",
    );
    cquery_platform_case(
        "cquery-test-guava-upgrade.zip",
        "jre",
        "cquery-test-guava-upgrade-jre-impacted-targets.txt",
    );
}

#[test]
fn cquery_android_source_change_platforms() {
    cquery_platform_case(
        "cquery-test-android-code-change.zip",
        "android",
        "cquery-test-android-code-change-android-impacted-targets.txt",
    );
    cquery_platform_case(
        "cquery-test-android-code-change.zip",
        "jre",
        "cquery-test-android-code-change-jre-impacted-targets.txt",
    );
}

#[test]
fn bzlmod_show_repo_and_external_filtering() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = extract_fixture("bzlmod-show-repo-test-1.zip");
    let second = extract_fixture("bzlmod-show-repo-test-2.zip");
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let default_result = output.path().join("default.txt");
    let opt_out = output.path().join("opt-out.txt");
    assert!(generate(first.path(), &from, &[]).status.success());
    assert!(generate(second.path(), &to, &[]).status.success());
    assert!(impacted(second.path(), &from, &to, &default_result, &[])
        .status
        .success());
    let default_labels = read_lines(&default_result);
    assert!(default_labels
        .iter()
        .any(|label| label.contains("guava-user")));
    assert!(!default_labels
        .iter()
        .any(|label| label.starts_with("//external:")));
    assert!(!default_labels.iter().any(|label| {
        label.contains("bazel-diff-integration-lib")
            && !label.contains("libbazel-diff-integration-lib")
    }));
    assert!(impacted(
        second.path(),
        &from,
        &to,
        &opt_out,
        &["--no-excludeExternalTargets"],
    )
    .status
    .success());
    assert!(read_lines(&opt_out)
        .iter()
        .any(|label| label.starts_with("//external:")));
}

#[test]
fn transitive_external_repo_change_reaches_consumer() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("bzlmod_transitive_external");
    let second = copy_workspace("bzlmod_transitive_external");
    fs::write(second.path().join("inner/data.txt"), "world\n").unwrap();
    let impacted = diff_local(first.path(), second.path(), &[]);
    assert!(impacted.contains("//:consume") || impacted.contains("@@//:consume"));
}

/// cquery prints canonical labels (`@@inner+//:data`) where query prints apparent ones
/// (`@inner//:data`). The synthetic `//external:inner` node is named by the apparent name, so
/// until the canonical name was translated the edge from `@@outer+//:lib` to the node was dropped
/// and a change inside `inner` never reached `//:consume` under `--useCquery`. hashbench
/// (2026-10-18) hit the same gap on a rules_multitool lockfile bump.
#[test]
fn transitive_external_repo_change_reaches_consumer_cquery() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("bzlmod_transitive_external");
    let second = copy_workspace("bzlmod_transitive_external");
    fs::write(second.path().join("inner/data.txt"), "world\n").unwrap();
    let impacted = diff_local(first.path(), second.path(), &["--useCquery"]);
    assert!(
        impacted.contains("//:consume") || impacted.contains("@@//:consume"),
        "{impacted:?}"
    );
}

#[test]
fn remote_proto_bump_impacts_consumer() {
    let first = copy_workspace("proto_external_version_bump");
    let second = copy_workspace("proto_external_version_bump");
    edit(&second.path().join("MODULE.bazel"), |text| {
        text.replace(
            "bazel_dep(name = \"proto_dep\", version = \"1.0.0\")",
            "bazel_dep(name = \"proto_dep\", version = \"2.0.0\")",
        )
    });
    edit(&second.path().join("proto_dep/MODULE.bazel"), |text| {
        text.replace("version = \"1.0.0\"", "version = \"2.0.0\"")
    });
    edit(&second.path().join("proto_dep/greeting.proto"), |text| {
        text.replace(
            "message Greeting {\n  string message = 1;\n}",
            "message Greeting {\n  string message = 1;\n  string locale = 2;\n}",
        )
    });
    let impacted = diff_local(
        first.path(),
        second.path(),
        &["--fineGrainedHashExternalRepos", "@proto_dep"],
    );
    assert!(impacted.contains("//:consumer") || impacted.contains("@@//:consumer"));
    assert!(impacted
        .iter()
        .any(|label| label.ends_with("proto_dep//:greeting_java_proto")));
}

// Hub-and-spoke external repos -- e.g. rules_python's pip_parse in WORKSPACE mode, with a `@pip`
// hub of alias packages and one `@pip_<pkg>` spoke repo per package -- keep the change-detection
// signal in the spoke: bumping a pinned version rewrites the spoke repository rule's attrs, so only
// `//external:pip_numpy` flips. Users list the hub their BUILD files reference (`@pip`) rather than
// every generated spoke, and the consumer reaches the flipping seed through
// `@pip//numpy:pkg -> @pip_numpy//:lib -> //external:pip_numpy`. Fine-grained matching used to be an
// unbounded string prefix, so the spoke label looked like it belonged to the hub and was never
// rewritten to its seed, silently dropping consumers from the impacted set.
#[test]
fn hub_spoke_version_bump_impacts_consumer_behind_fine_grained_hub() {
    // WORKSPACE-only fixture: Bazel 9 dropped WORKSPACE, so `@pip` does not exist there at all.
    // The matching logic itself stays pinned on every version by the `hash` unit tests.
    if !supports_workspace() {
        return;
    }
    let first = copy_workspace("hub_spoke_external");
    let second = copy_workspace("hub_spoke_external");
    edit(&second.path().join("WORKSPACE"), |text| {
        text.replace("version = \"1.0\"", "version = \"2.0\"")
    });
    let impacted = diff_local(
        first.path(),
        second.path(),
        &["--fineGrainedHashExternalRepos", "@pip"],
    );
    // Sanity: if the spoke seed stops flipping the fixture no longer reproduces the signal and the
    // consumer assertion below proves nothing.
    assert!(impacted.contains("//external:pip_numpy"), "{impacted:?}");
    assert!(
        impacted.contains("//:consumer") || impacted.contains("@@//:consumer"),
        "{impacted:?}"
    );
}

// The Bzlmod form of the case above, with the hub named by its canonical name. `query` prints the
// apparent `@crates//:a`, which must still count as fine-grained: otherwise the input falls back to
// the hub's `//external:crates` node, which depends on every spoke, and bumping one crate impacts
// every consumer of the hub. The hub's own `versions.txt`, also named by its apparent label, must
// be hashed by content under either name, or `//:uses_versions` misses the bump.
#[test]
fn hub_named_by_canonical_name_is_fine_grained_under_its_apparent_name() {
    if !supports_mod_show_repo() {
        return;
    }
    let first = copy_workspace("fine_grained_hub_by_canonical_name");
    let second = copy_workspace("fine_grained_hub_by_canonical_name");
    edit(&second.path().join("versions.json"), |text| {
        text.replace("\"a\": \"1.0\"", "\"a\": \"2.0\"")
    });
    for repo in ["@crates", "@@+crates+crates"] {
        let impacted = diff_local(
            first.path(),
            second.path(),
            &["--fineGrainedHashExternalRepos", repo],
        );
        let main: BTreeSet<_> = impacted
            .iter()
            .filter(|label| !label.starts_with("//external:") && !label.starts_with("@@crates"))
            .map(|label| label.trim_start_matches("@@").to_owned())
            .filter(|label| label.starts_with("//"))
            .collect();
        assert_eq!(
            main,
            BTreeSet::from(["//:uses_a".to_owned(), "//:uses_versions".to_owned()]),
            "{repo}: {impacted:?}"
        );
    }
}

// Module `ext`'s extension generates the `@crates` hub and its spokes, whose targets use a rule
// `ext` defines. In query mode `//external:ext` stands in for everything its extensions generate,
// so a fine-grained spoke target reaching `ext` through it changes whenever any other spoke does.
// With the extension listed in `--labelOnlyModuleExtensions`, bumping spoke `b` impacts only
// `//:uses_b`, while a change to `ext` itself still impacts both consumers. Without it, the
// conservative edge stays and the bump impacts both.
#[test]
fn fine_grained_spoke_does_not_reach_its_siblings_through_the_owning_module() {
    if !supports_mod_show_repo() {
        return;
    }
    let fine_grained = [
        "--fineGrainedHashExternalRepos",
        "@crates,@@ext++crates+crates__a",
        "--labelOnlyModuleExtensions",
        "@ext//:crates.bzl%crates",
    ];
    let main = |impacted: BTreeSet<String>| -> BTreeSet<String> {
        impacted
            .into_iter()
            .map(|label| label.trim_start_matches("@@").to_owned())
            .filter(|label| label.starts_with("//") && !label.starts_with("//external:"))
            .collect()
    };
    let first = copy_workspace("extension_spokes_through_owner");
    let bumped = copy_workspace("extension_spokes_through_owner");
    edit(&bumped.path().join("versions.json"), |text| {
        text.replace("\"b\": \"1.0\"", "\"b\": \"2.0\"")
    });
    let impacted = main(diff_local(first.path(), bumped.path(), &fine_grained));
    assert_eq!(impacted, BTreeSet::from(["//:uses_b".to_owned()]));
    let impacted = main(diff_local(first.path(), bumped.path(), &fine_grained[..2]));
    assert_eq!(
        impacted,
        BTreeSet::from(["//:uses_a".to_owned(), "//:uses_b".to_owned()])
    );

    let changed_rule = copy_workspace("extension_spokes_through_owner");
    fs::write(changed_rule.path().join("ext/tool.sh"), "exit 1\n").unwrap();
    let impacted = main(diff_local(first.path(), changed_rule.path(), &fine_grained));
    assert_eq!(
        impacted,
        BTreeSet::from(["//:uses_a".to_owned(), "//:uses_b".to_owned()])
    );
}

// Issue #603. Module `ext` owns the `@crates` extension, whose spokes are named after their
// version the way rules_js's `npm` names `npm__user-agents__2.1.229`, so a bump replaces spoke `b`
// rather than changing it. In query mode `//external:ext` depends on every repository its
// extensions generate, so the bump reaches every target that uses `ext`'s own rule (`//:uses_ext`,
// like a js_binary) and every target of a module with a `bazel_dep` path to `ext`
// (`//:uses_dependent`, whose `dependent` reaches `ext` through `middle`, like rules_jest reaching
// gazelle). Neither names anything in `@crates`. With the extension listed in
// `--labelOnlyModuleExtensions`, `ext` no longer stands in for its repositories, so the bump
// impacts only the hub's consumers, with or without fine-graining; a change to `ext` itself must
// still reach both.
#[test]
fn label_only_extension_bump_does_not_reach_users_of_the_owner_or_its_dependents() {
    if !supports_mod_show_repo() {
        return;
    }
    let label_only = ["--labelOnlyModuleExtensions", "@ext//:crates.bzl%crates"];
    let fine_grained = [
        label_only[0],
        label_only[1],
        "--fineGrainedHashExternalRepos",
        "@crates",
    ];
    let main = |impacted: BTreeSet<String>| -> BTreeSet<String> {
        impacted
            .into_iter()
            .map(|label| label.trim_start_matches("@@").to_owned())
            .filter(|label| label.starts_with("//") && !label.starts_with("//external:"))
            .collect()
    };
    let set = |labels: &[&str]| -> BTreeSet<String> {
        labels.iter().map(|label| label.to_string()).collect()
    };
    let first = copy_workspace("extension_owner_dependent_module");
    let bumped = copy_workspace("extension_owner_dependent_module");
    edit(&bumped.path().join("versions.json"), |text| {
        text.replace("\"b\": \"1.0\"", "\"b\": \"2.0\"")
    });
    // Without the opt-in the conservative owner edge stays and the bump reaches both, which is
    // what the issue reports.
    let impacted = main(diff_local(first.path(), bumped.path(), &[]));
    assert!(
        impacted.contains("//:uses_ext") && impacted.contains("//:uses_dependent"),
        "{impacted:?}"
    );

    // Without fine-graining every consumer of the `@crates` hub sees the bump.
    let impacted = main(diff_local(first.path(), bumped.path(), &label_only));
    assert_eq!(impacted, set(&["//:uses_a", "//:uses_b"]), "label-only");
    let impacted = main(diff_local(first.path(), bumped.path(), &fine_grained));
    assert_eq!(impacted, set(&["//:uses_b"]), "label-only, fine-grained");

    // `dependent` and `//:uses_ext` really depend on `ext`: a change to its own tree reaches them.
    let changed_owner = copy_workspace("extension_owner_dependent_module");
    fs::write(changed_owner.path().join("ext/tool.sh"), "exit 1\n").unwrap();
    for flags in [&label_only[..], &fine_grained[..]] {
        let impacted = main(diff_local(first.path(), changed_owner.path(), flags));
        assert!(
            impacted.contains("//:uses_ext") && impacted.contains("//:uses_dependent"),
            "{flags:?}: {impacted:?}"
        );
    }
}

fn partial_hashes(
    root: &std::path::Path,
    out: &std::path::Path,
    name: &str,
    base: Option<&std::path::Path>,
    options: &[&str],
) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let hashes = out.join(format!("{name}.json"));
    let edges = out.join(format!("{name}-edges.json"));
    let edge_arg = edges.display().to_string();
    let base_arg = base.map(|path| path.display().to_string());
    let mut args = options.to_vec();
    args.extend(["--partialGraph", "-d", &edge_arg]);
    if let Some(base) = &base_arg {
        args.extend(["--baseHashes", base]);
    }
    let result = generate(root, &hashes, &args);
    let log = String::from_utf8_lossy(&result.stderr).into_owned();
    assert!(result.status.success(), "{name}: {log}");
    (hashes, edges, log)
}

#[test]
fn fine_aliases_acquire_hidden_libraries_and_definition_sources() {
    if !supports_mod_show_repo() {
        return;
    }
    fn external_key<V>(map: &std::collections::BTreeMap<String, V>, suffix: &str) -> String {
        let keys = map
            .keys()
            .filter(|key| key.starts_with('@') && key.ends_with(suffix))
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 1, "expected one {suffix}: {keys:?}");
        keys[0].as_str().to_owned()
    }
    fn direct(hash: &str) -> &str {
        hash.rsplit_once('~').unwrap().1
    }

    let workspace = copy_workspace("fine_alias_acquisition");
    let root = workspace.path();
    let out = tempfile::tempdir().unwrap();
    let build_path = root.join("library/BUILD.bazel");
    let build = fs::read_to_string(&build_path).unwrap();
    let implementation_path = root.join("library/defs/library.bzl");
    let implementation = fs::read_to_string(&implementation_path).unwrap();
    let unrelated_path = root.join("unrelated/rules.bzl");
    let unrelated = fs::read_to_string(&unrelated_path).unwrap();
    let modes: [(&str, &[&str]); 2] = [
        ("fine", &["--fineGrainedHashExternalRepos=@public"]),
        (
            "excluded",
            &[
                "--fineGrainedHashExternalRepos=@public",
                "--excludeExternalTargets",
            ],
        ),
    ];
    let mut snapshots = std::collections::BTreeMap::new();
    for (state, data, other, unrelated_edit, product) in [
        ("base", "before\n", false, false, "before"),
        ("source", "after\n", false, false, "after"),
        ("definition", "before\n", true, false, "other"),
        ("implementation", "before\n", false, false, "other"),
        ("unrelated", "before\n", false, true, "before"),
    ] {
        fs::write(root.join("library/data.txt"), data).unwrap();
        fs::write(
            &build_path,
            if other {
                build.replace("selected = \"data.txt\"", "selected = \"other.txt\"")
            } else {
                build.clone()
            },
        )
        .unwrap();
        fs::write(
            &implementation_path,
            if state == "implementation" {
                implementation.replace(
                    "file.basename == ctx.attr.selected",
                    "file.basename != ctx.attr.selected",
                )
            } else {
                implementation.clone()
            },
        )
        .unwrap();
        fs::write(
            &unrelated_path,
            format!(
                "{unrelated}{}",
                if unrelated_edit {
                    "\n# unrelated edit\n"
                } else {
                    ""
                }
            ),
        )
        .unwrap();
        assert_product(root, "//:consumer", "consumer.txt", product);
        for (mode, flags) in modes {
            let hashes = out.path().join(format!("{state}-{mode}.json"));
            let edges = out.path().join(format!("{state}-{mode}-edges.json"));
            let mut args = flags.to_vec();
            args.extend(["--includeTargetType", "-d", edges.to_str().unwrap()]);
            let generated = generate(root, &hashes, &args);
            assert!(
                generated.status.success(),
                "{state} {mode}: {}",
                String::from_utf8_lossy(&generated.stderr)
            );
            snapshots.insert((state, mode), (hashes, edges));
        }
    }
    for (mode, _) in modes {
        for (state, expected) in [
            ("source", true),
            ("definition", true),
            ("implementation", true),
            ("unrelated", false),
        ] {
            let result = out.path().join(format!("{state}-{mode}-impact.txt"));
            let impact = impacted(
                root,
                &snapshots[&("base", mode)].0,
                &snapshots[&(state, mode)].0,
                &result,
                &[],
            );
            assert!(
                impact.status.success(),
                "{state} {mode}: {}",
                String::from_utf8_lossy(&impact.stderr)
            );
            let selected = read_lines(&result);
            assert_eq!(
                selected.contains("//:consumer"),
                expected,
                "{state} {mode}: {selected:?}"
            );
            if state == "unrelated" {
                assert!(selected.contains("//unrelated:probe"));
            }
        }
        let base = hashes(&snapshots[&("base", mode)].0);
        let source = hashes(&snapshots[&("source", mode)].0);
        let definition = hashes(&snapshots[&("definition", mode)].0);
        let data = external_key(&base, "library//:data.txt");
        let other = external_key(&base, "library//:other.txt");
        let build = external_key(&base, "library//:BUILD.bazel");
        let library_hash = external_key(&base, "library//:lib");
        assert_ne!(direct(&base[&data]), direct(&source[&data]));
        assert_ne!(direct(&base[&library_hash]), direct(&source[&library_hash]));
        assert_eq!(direct(&base[&other]), direct(&source[&other]));
        assert_eq!(direct(&base[&build]), direct(&source[&build]));
        assert_ne!(direct(&base[&build]), direct(&definition[&build]));
        assert_eq!(direct(&base[&data]), direct(&definition[&data]));

        let implementation = hashes(&snapshots[&("implementation", mode)].0);
        let bzl = external_key(&base, "library//defs:library.bzl");
        assert_ne!(direct(&base[&bzl]), direct(&implementation[&bzl]));
        for file in [&build, &data, &other] {
            assert_eq!(direct(&base[file]), direct(&implementation[file]));
        }

        let edges = dep_edges(&snapshots[&("base", mode)].1);
        let public = external_key(&edges, "public//:forward");
        let hidden = external_key(&edges, "hidden//:forward");
        let library = external_key(&edges, "library//:lib");
        assert!(edges["//:consumer"].contains(&public));
        assert!(edges[&public].contains(&hidden));
        assert!(edges[&hidden].contains(&library));
        external_key(&base, "hidden//:BUILD.bazel");
    }
}

#[test]
fn fine_aliases_track_the_selected_destination_with_fixed_inputs() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("fine_alias_acquisition");
    let root = workspace.path();
    let output = tempfile::tempdir().unwrap();
    edit(&root.join("library/BUILD.bazel"), |text| {
        format!("{text}\nexports_files([\"other.txt\"])\n")
    });
    edit(&root.join("MODULE.bazel"), |text| {
        text.replace(
            r#"use_repo(aliases, "public")"#,
            r#"use_repo(aliases, "public", "hidden")"#,
        )
    });
    let mut snapshots = [Vec::new(), Vec::new()];
    let first = "@hidden//:forward";
    let other = "@library//:other.txt";
    for (name, default, alternate, sibling, product) in [
        ("base", first, other, first, "before"),
        ("sibling", first, other, other, "before"),
        ("swapped", other, first, other, "other"),
    ] {
        fs::write(
            root.join("public.BUILD"),
            format!(
                r#"config_setting(name = "alt", values = {{"define": "choice=alt"}})
alias(
    name = "forward",
    actual = select({{":alt": "{alternate}", "//conditions:default": "{default}"}}),
    visibility = ["//visibility:public"],
)
alias(name = "sibling", actual = "{sibling}")
"#
            ),
        )
        .unwrap();
        assert_product(root, "//:consumer", "consumer.txt", product);
        for (idx, fine) in ["@public", "@public,@hidden,@library"]
            .into_iter()
            .enumerate()
        {
            let out = output.path().join(format!("{name}-{idx}.json"));
            let edges = output.path().join(format!("{name}-{idx}-edges.json"));
            let generated = generate(
                root,
                &out,
                &[
                    &format!("--fineGrainedHashExternalRepos={fine}"),
                    "--bazelCommandOptions=--proto:output_rule_attrs=tags",
                    "-d",
                    edges.to_str().unwrap(),
                ],
            );
            assert!(
                generated.status.success(),
                "{}",
                String::from_utf8_lossy(&generated.stderr)
            );
            let graph = dep_edges(&edges);
            let key = graph
                .keys()
                .find(|k| k.ends_with("public//:forward"))
                .unwrap();
            snapshots[idx].push((hashes(&out)[key].clone(), graph[key].clone(), out));
        }
    }
    for (idx, profile) in snapshots.into_iter().enumerate() {
        let [(before, inputs, first), (unchanged, sibling_inputs, _), (after, swapped_inputs, second)] =
            profile.as_slice()
        else {
            unreachable!()
        };
        assert_eq!(inputs, sibling_inputs);
        assert_eq!(inputs, swapped_inputs);
        assert_eq!(before.rsplit('~').next(), unchanged.rsplit('~').next());
        assert_ne!(
            before.rsplit('~').next(),
            after.rsplit('~').next(),
            "fine profile {idx}"
        );
        let out = output.path().join("impact.txt");
        assert!(impacted(root, first, second, &out, &[]).status.success());
        assert!(read_lines(&out).contains("//:consumer"));
    }
}

#[test]
fn fine_aliases_preserve_partial_rebuilds() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("fine_alias_acquisition");
    let root = workspace.path();
    assert_product(root, "//:consumer", "consumer.txt", "before");
    init_git(root);
    let out = tempfile::tempdir().unwrap();
    let partial = |name: &str, base: Option<&std::path::Path>| {
        let hashes = out.path().join(format!("{name}.json"));
        let edges = out.path().join(format!("{name}-edges.json"));
        let edge_arg = edges.display().to_string();
        let base_arg = base.map(|path| path.display().to_string());
        let mut args = vec![
            "--fineGrainedHashExternalRepos=@public",
            "--excludeExternalTargets",
            "--partialGraph",
            "-d",
            &edge_arg,
        ];
        if let Some(base) = &base_arg {
            args.extend(["--baseHashes", base]);
        }
        let result = generate(root, &hashes, &args);
        let log = String::from_utf8_lossy(&result.stderr).into_owned();
        assert!(result.status.success(), "{name}: {log}");
        (hashes, edges, log)
    };
    partial("warm", None);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        commit(root, "settle repository metadata");
    }
    let lock_path = root.join("MODULE.bazel.lock");
    let lock = fs::read(&lock_path).unwrap();
    let (base, _, _) = partial("base", None);
    assert_eq!(fs::read(&lock_path).unwrap(), lock);
    fs::write(root.join("library/data.txt"), "after\n").unwrap();
    assert_product(root, "//:consumer", "consumer.txt", "after");
    assert_eq!(fs::read(&lock_path).unwrap(), lock);
    assert_eq!(git(root, &["diff", "--name-only"]), "library/data.txt");
    assert!(git(root, &["ls-files", "--others", "--exclude-standard"]).is_empty());
    let (reused, reused_edges, log) = partial("reused", Some(&base));
    assert!(
        log.contains("full generation")
            && log.contains("read while loading")
            && log.contains("library"),
        "{log}"
    );
    let (fresh, fresh_edges, _) = partial("fresh", None);
    let saved_source = |path: &std::path::Path| {
        let data: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let sources = data["metadata"]["partialGraph"]["externalSources"]
            .as_object()
            .expect("external source recipes");
        let (label, digest) = sources
            .iter()
            .find(|(label, _)| label.ends_with("library//:data.txt"))
            .expect("the library source behind the fine alias");
        let hash = data["hashes"][label].as_str().expect("library source hash");
        let direct = hash.rsplit_once('~').unwrap().1;
        (digest.as_str().unwrap().to_owned(), direct.to_owned())
    };
    let (base_source, base_direct) = saved_source(&base);
    let (fresh_source, fresh_direct) = saved_source(&fresh);
    assert_ne!(base_source, fresh_source);
    assert_ne!(base_direct, fresh_direct);
    assert_eq!(fs::read(&reused).unwrap(), fs::read(&fresh).unwrap());
    assert_eq!(dep_edges(&reused_edges), dep_edges(&fresh_edges));
    assert!(dep_edges(&fresh_edges).iter().any(|(label, inputs)| {
        label.ends_with("hidden//:forward")
            && inputs.iter().any(|input| input.ends_with("library//:lib"))
    }));
    let selected = out.path().join("selected.txt");
    let result = impacted(root, &base, &reused, &selected, &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(read_lines(&selected).contains("//:consumer"));
}

#[test]
fn fine_aliases_honor_query_failure_mode() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("fine_alias_acquisition");
    let root = workspace.path();
    edit(&root.join("public.BUILD"), |text| {
        format!("{text}\nalias(name = \"dangling\", actual = \"@hidden//:missing\")\n")
    });
    edit(&root.join("BUILD.bazel"), |text| {
        format!("{text}\nfilegroup(name = \"uses_dangling\", srcs = [\"@public//:dangling\"])\n")
    });
    let out = tempfile::tempdir().unwrap();
    let strict = out.path().join("strict.json");
    let flags = [
        "--fineGrainedHashExternalRepos=@public",
        "--excludeExternalTargets",
    ];
    let result = generate(root, &strict, &flags);
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{error}");
    assert!(
        error.contains("failed to acquire ordinary alias inputs") && error.contains("//:missing"),
        "{error}"
    );
    assert!(!strict.exists());

    let kept = out.path().join("kept.json");
    let edges = out.path().join("kept-edges.json");
    let mut args = flags.to_vec();
    args.extend(["--keep_going", "-d", edges.to_str().unwrap()]);
    let result = generate(root, &kept, &args);
    let warning = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{warning}");
    assert!(
        warning.contains("keeping ordinary repository fallbacks for incomplete alias discovery"),
        "{warning}"
    );
    assert!(!dep_edges(&edges)
        .keys()
        .any(|label| label.ends_with("hidden//:forward")));
}

const ISOLATED_ROOT: &[&str] = &["--isolatedDependencyRoot", "@hub"];

fn assert_product(workspace: &std::path::Path, target: &str, file: &str, expected: &str) {
    let build = std::process::Command::new(bazel())
        .current_dir(workspace)
        .args(["build", target])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert_eq!(
        fs::read_to_string(workspace.join("bazel-bin").join(file))
            .unwrap()
            .trim(),
        expected
    );
}

#[test]
fn local_archive_build_file_reaches_only_its_own_repository_and_readers() {
    if !supports_mod_show_repo() {
        return;
    }
    let workspace = copy_workspace("local_archive_build_file");
    let root = workspace.path();
    let archive = fixture("integration-test-1.zip");
    edit(&root.join("MODULE.bazel"), |text| {
        text.replace("ARCHIVE_URL", &format!("file://{}", archive.display()))
    });
    let out = tempfile::tempdir().unwrap();
    let template = fs::read_to_string(root.join("overlays/overlay.tpl")).unwrap();
    let base = out.path().join("base.json");
    for (arm, active, neighbor) in [
        ("base", "A", "A"),
        ("active", "B", "A"),
        ("neighbor", "A", "B"),
    ] {
        for (file, value) in [("active.rules", active), ("neighbor.rules", neighbor)] {
            fs::write(
                root.join("overlays").join(file),
                template.replace("VALUE", value),
            )
            .unwrap();
        }
        let hashes = out.path().join(format!("{arm}.json"));
        let generated = generate(root, &hashes, &[]);
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        // The resolved tool and the direct reader use the same archive output.
        assert_product(root, "//:products", "tool_user.txt", active);
        assert_eq!(
            fs::read_to_string(root.join("bazel-bin/plain.txt"))
                .unwrap()
                .trim(),
            active
        );
        assert_eq!(
            fs::read_to_string(root.join("bazel-bin/other.txt"))
                .unwrap()
                .trim(),
            neighbor
        );
        if arm == "base" {
            continue;
        }
        let before: serde_json::Value = serde_json::from_slice(&fs::read(&base).unwrap()).unwrap();
        let after: serde_json::Value = serde_json::from_slice(&fs::read(&hashes).unwrap()).unwrap();
        for (repo, expected) in [
            ("//external:sdk", arm == "active"),
            ("//external:neighbor", arm == "neighbor"),
        ] {
            let before = before["hashes"][repo]
                .as_str()
                .unwrap()
                .rsplit_once('~')
                .unwrap()
                .1;
            let after = after["hashes"][repo]
                .as_str()
                .unwrap()
                .rsplit_once('~')
                .unwrap()
                .1;
            assert_eq!(before != after, expected, "{arm}: {repo}");
        }
        let affected = out.path().join(format!("{arm}.txt"));
        let result = impacted(root, &base, &hashes, &affected, &[]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let rows = fs::read_to_string(affected).unwrap();
        let changed: BTreeSet<_> = rows
            .lines()
            .map(|label| label.trim_start_matches('@'))
            .collect();
        for (label, expected) in [
            ("//:registration", arm == "active"),
            ("//:plain", arm == "active"),
            ("//:other", arm == "neighbor"),
        ] {
            assert_eq!(changed.contains(label), expected, "{arm}: {label}");
        }
    }

    init_git(root);
    let partial = |name: &str, base: Option<&std::path::Path>| {
        partial_hashes(root, out.path(), name, base, &[])
    };
    partial("warm", None);
    if !git(root, &["status", "--porcelain"]).is_empty() {
        commit(root, "settle repository metadata");
    }
    let lock_path = root.join("MODULE.bazel.lock");
    let lock = fs::read(&lock_path).unwrap();
    let (base, _, _) = partial("partial-base", None);
    assert_eq!(fs::read(&lock_path).unwrap(), lock);
    fs::write(
        root.join("overlays/active.rules"),
        template.replace("VALUE", "B"),
    )
    .unwrap();
    assert_product(root, "//:plain", "plain.txt", "B");
    assert_eq!(fs::read(&lock_path).unwrap(), lock);
    assert_eq!(git(root, &["diff", "--name-only"]), "overlays/active.rules");
    assert!(git(root, &["ls-files", "--others", "--exclude-standard"]).is_empty());
    let (reused, reused_edges, log) = partial("reused", Some(&base));
    assert!(
        log.contains("full generation")
            && log.contains("read while loading")
            && log.contains("overlays"),
        "{log}"
    );
    let (fresh, fresh_edges, _) = partial("fresh", None);
    assert_eq!(fs::read(&reused).unwrap(), fs::read(&fresh).unwrap());
    assert_eq!(dep_edges(&reused_edges), dep_edges(&fresh_edges));
    let selected = out.path().join("partial-selected.txt");
    let result = impacted(root, &base, &reused, &selected, &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let selected = read_lines(&selected);
    assert!(selected.contains("//:plain"), "{selected:?}");
    assert!(!selected.contains("//:other"), "{selected:?}");
}

#[test]
fn isolated_dependency_root_reaches_alias_direct_and_build_load_readers() {
    if !supports_mod_show_repo() {
        return;
    }
    let base = copy_workspace("isolated_dependency_root");
    let head = copy_workspace("isolated_dependency_root");
    edit(&head.path().join("settings.json"), |s| {
        s.replace("before", "after")
    });
    assert!(diff_local(base.path(), head.path(), &[]).contains("//outside:uses_owner"));
    let changed = diff_local(base.path(), head.path(), ISOLATED_ROOT);
    for reader in ["//:alias", "//:direct", "//:loaded"] {
        assert!(changed.contains(reader), "missing {reader}: {changed:?}");
    }
    assert!(!changed.contains("//outside:uses_owner"), "{changed:?}");
    assert_product(base.path(), "//:direct", "direct.txt", "before");
    assert_product(head.path(), "//:direct", "direct.txt", "after");

    let comment = copy_workspace("isolated_dependency_root");
    edit(&comment.path().join("settings.json"), |s| {
        s.replace("before", "before-comment")
    });
    let maps = tempfile::tempdir().unwrap();
    let direct = |ws: &std::path::Path, name: &str| {
        let map = maps.path().join(name);
        assert!(generate(ws, &map, ISOLATED_ROOT).status.success());
        let data = hashes(&map);
        (
            data["@hub//:defs.bzl"].clone(),
            data["//external:hub"].split_once('~').unwrap().1.to_owned(),
        )
    };
    let before = direct(base.path(), "base.json");
    let after = direct(comment.path(), "comment.json");
    let captured: serde_json::Value =
        serde_json::from_slice(&fs::read(maps.path().join("base.json")).unwrap()).unwrap();
    let graph: serde_json::Value =
        serde_json::from_str(captured["metadata"]["moduleGraphJson"].as_str().unwrap()).unwrap();
    assert_eq!(graph["key"], "<root>");
    assert!(
        graph["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|module| module["dependencies"].as_array().is_some_and(Vec::is_empty)),
        "{graph}"
    );
    assert_ne!(before.0, after.0);
    assert_ne!(before.1, after.1);
    assert_product(comment.path(), "//:direct", "direct.txt", "before");

    edit(&base.path().join("MODULE.bazel"), |module| {
        module.replace(
            "use_repo(family, \"hub\")",
            "use_repo(family, \"hub\", \"app\")",
        )
    });
    let refused = maps.path().join("refused.json");
    let output = generate(base.path(), &refused, ISOLATED_ROOT);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("--isolatedDependencyRoot=@hub")
            && stderr.contains("extra imported repository")
            && stderr.contains("+app"),
        "{stderr}"
    );
    assert!(!refused.exists(), "failed admission wrote a hash file");
    let unflagged = maps.path().join("unflagged.json");
    let output = generate(base.path(), &unflagged, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(hashes(&unflagged).contains_key("//:direct"));
}

#[test]
fn isolated_dependency_root_tracks_repository_read_bytes() {
    if !supports_mod_show_repo() {
        return;
    }
    let base = copy_workspace("isolated_dependency_root");
    for (input, contents, target, product, initial, expected) in [
        (
            "registry.txt",
            "changed",
            "//:reads_registry",
            "registry-output.txt",
            "original",
            "changed",
        ),
        (
            "alias.txt",
            "changed",
            "//:reads_alias",
            "alias-output.txt",
            "absent",
            "changed",
        ),
        // No queried output changes. Only the typed repository input changes
        // from absent to present at the alias's path.
        (
            "alias.txt",
            "absent\nnew file",
            "//:reads_alias",
            "alias-output.txt",
            "absent",
            "absent",
        ),
    ] {
        let head = copy_workspace("isolated_dependency_root");
        fs::write(head.path().join(input), contents).unwrap();
        let changed = diff_local(base.path(), head.path(), ISOLATED_ROOT);
        assert!(changed.contains(target), "{input}: {changed:?}");
        assert!(
            !changed.contains("//outside:uses_owner"),
            "{input}: {changed:?}"
        );
        assert_product(base.path(), target, product, initial);
        assert_product(head.path(), target, product, expected);
    }
}

#[test]
fn isolated_dependency_root_keeps_hidden_spoke_changes() {
    if !supports_mod_show_repo() {
        return;
    }
    let base = copy_workspace("isolated_dependency_root");
    let head = copy_workspace("isolated_dependency_root");
    edit(&head.path().join("settings.json"), |s| {
        s.replace("\"1\"", "\"2\"")
    });
    let changed = diff_local(base.path(), head.path(), ISOLATED_ROOT);
    assert!(
        changed.contains("//:alias") && changed.contains("//:direct"),
        "{changed:?}"
    );
    assert!(!changed.contains("//outside:uses_owner"), "{changed:?}");
}

#[test]
fn toolchain_declarations_follow_their_implementations() {
    let workspace = copy_workspace("extension_toolchain_through_owner");
    let root = workspace.path();
    edit(&root.join("MODULE.bazel"), |module| {
        format!("{module}\nregister_toolchains(\"//declaration:registration\")\n")
    });
    let out = tempfile::tempdir().unwrap();
    let before = out.path().join("before.json");
    let after = out.path().join("after.json");
    let edges = out.path().join("edges.json");
    let result = out.path().join("impacted.json");
    let args = [
        "--bazelCommandOptions=--noproto:flatten_selects",
        "--includeTargetType",
        "-d",
        edges.to_str().unwrap(),
    ];
    for (hashes, value) in [(&before, "before"), (&after, "after")] {
        if value == "after" {
            edit(&root.join("declaration/BUILD.bazel"), |text| {
                text.replace("\"before\"", "\"after\"")
            });
        }
        assert_product(
            root,
            "//declaration:consumer",
            "declaration/consumer.txt",
            value,
        );
        assert!(generate(root, hashes, &args).status.success());
    }
    assert!(impacted(
        root,
        &before,
        &after,
        &result,
        &["-d", edges.to_str().unwrap()]
    )
    .status
    .success());
    let changes: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
    let distance = |name: &str| {
        changes
            .iter()
            .find(|row| row["label"] == name)
            .and_then(|row| row["targetDistance"].as_u64())
    };
    assert_eq!(distance("//declaration:primary"), Some(0));
    assert_eq!(distance("//declaration:registration"), Some(1));
    assert_eq!(distance("//declaration:choice"), Some(1));
    assert_eq!(distance("//declaration:other"), None);
    assert!(dep_edges(&edges)["//declaration:choice"].contains("//declaration:other"));
}

// Module `ext`'s extension generates `@app`, whose rule resolves a toolchain, and the repositories
// that toolchain can come from. A query never shows a resolved toolchain as an input, so with
// `@app` fine-grained the change has to reach `//:uses_app` through `ext`, however `ext` registers
// the toolchain: from a repository it imports, through an alias whose toolchain lives in a
// repository nobody imports, or by canonical label without importing anything. This is why
// `--labelOnlyModuleExtensions` is opt-in: an extension like this one must not be listed.
#[test]
fn fine_grained_extension_target_sees_a_toolchain_from_its_extension() {
    if !supports_mod_show_repo() {
        return;
    }
    for registration in [
        "use_repo(tools, \"tc\")\nregister_toolchains(\"@tc//:all\")",
        "use_repo(tools, \"tc_alias\")\nregister_toolchains(\"@tc_alias//:tc\")",
        "register_toolchains(\"@@ext++tools+tc//:tc\")",
    ] {
        let first = copy_workspace("extension_toolchain_through_owner");
        let bumped = copy_workspace("extension_toolchain_through_owner");
        for workspace in [&first, &bumped] {
            fs::write(
                workspace.path().join("ext/MODULE.bazel"),
                format!(
                    "module(name = \"ext\")\n\ntools = use_extension(\"//:tools.bzl\", \"tools\")\n{registration}\n"
                ),
            )
            .unwrap();
        }
        edit(&bumped.path().join("versions.json"), |text| {
            text.replace("1.0", "2.0")
        });
        let impacted = diff_local(
            first.path(),
            bumped.path(),
            &["--fineGrainedHashExternalRepos", "@app"],
        );
        assert!(
            impacted
                .iter()
                .any(|label| label == "//:uses_app" || label == "@@//:uses_app"),
            "{registration}: {impacted:?}"
        );
        if registration.starts_with("register_toolchains") {
            // The assertion is specifically about consumers of @app. The distinct @tc_impl
            // repository selected via a registered toolchain must still invalidate its user.
            // This other usage of the same extension is isolated and root-imported.
            for workspace in [&first, &bumped] {
                edit(&workspace.path().join("MODULE.bazel"), |module| {
                    format!("{module}\npeer = use_extension(\"@ext//:tools.bzl\", \"tools\", isolate = True)\nuse_repo(peer, peer_tc = \"tc\")\n")
                });
                fs::write(
                    workspace.path().join(".bazelrc"),
                    "common --experimental_isolated_extension_usages\n",
                )
                .unwrap();
            }
            let out = tempfile::tempdir().unwrap();
            let edges = out.path().join("edges.json");
            let opted_in = diff_local(
                first.path(),
                bumped.path(),
                &[
                    "--fineGrainedHashExternalRepos",
                    "@app",
                    "--isolatedDependencyRoot",
                    "@peer_tc",
                    "--depEdgesFile",
                    edges.to_str().unwrap(),
                ],
            );
            let deps: std::collections::BTreeMap<String, Vec<String>> =
                serde_json::from_slice(&fs::read(edges).unwrap()).unwrap();
            let owner = &deps["//external:ext"];
            assert!(
                !owner.iter().any(|label| label == "//external:peer_tc"),
                "{owner:?}"
            );
            assert!(
                owner
                    .iter()
                    .any(|label| label == "//external:ext++tools+tc"),
                "{owner:?}"
            );
            assert!(
                opted_in
                    .iter()
                    .any(|label| label.trim_start_matches("@@") == "//:uses_app"),
                "{opted_in:?}"
            );
        }
    }
}

fn diff_local(
    first: &std::path::Path,
    second: &std::path::Path,
    args: &[&str],
) -> BTreeSet<String> {
    let output = tempfile::tempdir().unwrap();
    let from = output.path().join("from.json");
    let to = output.path().join("to.json");
    let result = output.path().join("impacted.txt");
    assert!(generate(first, &from, args).status.success());
    assert!(generate(second, &to, args).status.success());
    assert!(impacted(second, &from, &to, &result, &[]).status.success());
    read_lines(&result)
}

#[test]
fn external_macro_stack_uses_returned_source() {
    if !supports_mod_show_repo() {
        return;
    }
    let base = copy_workspace("external_macro_stack");
    let changed = copy_workspace("external_macro_stack");
    edit(&changed.path().join("defs.bzl"), |text| {
        text.replace(
            "right, \"//conditions:default\": left",
            "left, \"//conditions:default\": right",
        )
    });
    assert_product(base.path(), "//:product", "product.txt", "left");
    assert_product(changed.path(), "//:product", "product.txt", "right");
    let impacted = diff_local(
        base.path(),
        changed.path(),
        &[
            "--fineGrainedHashExternalRepos=@bridge,@macros",
            "--bazelStartupOptions=--ignore_all_rc_files",
        ],
    );
    let contains = |name| {
        impacted
            .iter()
            .any(|label| label.trim_start_matches('@') == name)
    };
    assert!(contains("//:uses_bridge"), "{impacted:?}");
    assert!(!contains("//:unrelated"), "{impacted:?}");
}

#[test]
fn returned_external_source_reaches_consumer() {
    let workspace = copy_workspace("returned_external_source");
    let output = tempfile::tempdir().unwrap();
    let before = output.path().join("before.json");
    let after = output.path().join("after.json");
    let result = output.path().join("impacted.txt");
    let args = [
        "--useCquery",
        "--cqueryExpression",
        "deps(set(//:consumer //:unrelated))",
    ];
    for (contents, snapshot) in [("before\n", &before), ("after\n", &after)] {
        fs::write(workspace.path().join("value.txt"), contents).unwrap();
        assert!(generate(workspace.path(), snapshot, &args).status.success());
        assert!(std::process::Command::new(bazel())
            .current_dir(workspace.path())
            .args(["build", "//:consumer"])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            fs::read_to_string(workspace.path().join("bazel-bin/copied.txt")).unwrap(),
            contents
        );
    }
    assert!(impacted(workspace.path(), &before, &after, &result, &[])
        .status
        .success());
    let targets = read_lines(&result);
    assert!(
        targets.iter().any(|t| t.ends_with("//:mirror.txt")),
        "{targets:?}"
    );
    assert!(
        targets.contains("@@//:consumer") || targets.contains("//:consumer"),
        "{targets:?}"
    );
    assert!(!targets.contains("@@//:unrelated") && !targets.contains("//:unrelated"));
}
