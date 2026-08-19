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
        &[
            "--fineGrainedHashExternalRepos",
            "@@rules_jvm_external~~maven~maven",
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
        &[
            "--fineGrainedHashExternalRepos",
            "@@rules_jvm_external~~maven~maven",
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
