genrule(
    name = "cli_help_output",
    outs = [
        "help_root.txt",
        "help_generate_hashes.txt",
        "help_get_impacted_targets.txt",
    ],
    cmd = """
        BINARY=$(location //cli:bazel-diff)
        $$BINARY --help > $(location help_root.txt) 2>&1 || true
        $$BINARY generate-hashes --help > $(location help_generate_hashes.txt) 2>&1 || true
        $$BINARY get-impacted-targets --help > $(location help_get_impacted_targets.txt) 2>&1 || true
    """,
    tools = ["//cli:bazel-diff"],
)

py_binary(
    name = "generate-readme",
    srcs = ["generate_readme.py"],
    main = "generate_readme.py",
    data = [
        ":cli_help_output",
        "readme_template.md",
    ],
    python_version = "PY3",
)
