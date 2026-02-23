"""A rule that always fails during analysis to simulate analysis_test scenarios."""

def _failing_rule_impl(ctx):
    """Implementation that always fails."""
    fail("This target is designed to fail during analysis")

failing_rule = rule(
    implementation = _failing_rule_impl,
    attrs = {},
)
