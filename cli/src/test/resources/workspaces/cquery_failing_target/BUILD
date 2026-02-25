load(":failing_rule.bzl", "failing_rule")
load("@bazel_skylib//rules:write_file.bzl", "write_file")

# A normal target that should work fine
write_file(
    name = "normal_target",
    out = "normal.txt",
    content = ["This is a normal target"],
)

# A target designed to fail during analysis
# This simulates analysis_test targets that are meant to validate build failures
failing_rule(
    name = "failing_analysis_target",
)

# Another normal target that depends on the normal target
write_file(
    name = "dependent_target",
    out = "dependent.txt",
    content = ["This depends on normal_target"],
)
