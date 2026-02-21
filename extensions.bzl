"""Bzlmod extension that fetches repositories for formatting tools (e.g. ktfmt)."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_jar")

def _non_module_repositories_impl(_module_ctx):
    # Fetch ktfmt JAR from Maven Central
    # This replaces the removed fetch_ktfmt() from aspect_rules_lint v1.x
    http_jar(
        name = "ktfmt",
        integrity = "sha256-l/x/vRlNAan6RdgUfAVSQDAD1VusSridhNe7TV4/SN4=",
        url = "https://repo1.maven.org/maven2/com/facebook/ktfmt/0.46/ktfmt-0.46-jar-with-dependencies.jar",
    )

non_module_repositories = module_extension(_non_module_repositories_impl)
