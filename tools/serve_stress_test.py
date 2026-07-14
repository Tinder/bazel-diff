"""Unit tests for tools/serve_stress.py's argument parsing.

The stress harness itself needs git, bazel and a live serve process, so it is exercised by the
serve-stress* workflows rather than here. What *is* unit-testable -- and what broke the
serve-stress-real cron on both of its first two runs -- is the CLI surface: an option whose value is
itself a flag (`--real-clone-args --depth=50`) is rejected by argparse unless it is folded into the
`=` spelling. These tests pin both spellings, using the exact argv the workflow matrix produces.

Run via Bazel:
    bazel test //tools:serve_stress_test
"""

import contextlib
import io
import unittest

from serve_stress import build_parser, fold_flag_values, parse_args


def ci_argv(clone_args: str, equals_spelling: bool) -> list:
    """The argv .github/workflows/serve-stress-real.yml expands to for one matrix leg, in either the
    `--real-clone-args=<v>` spelling (what the workflow pins) or the space-separated one."""
    tail = ([f"--real-clone-args={clone_args}"] if equals_spelling
            else ["--real-clone-args", clone_args])
    return [
        "--bazel", "/home/runner/go/bin/bazelisk",
        "--real-repo-url", "https://github.com/bazelbuild/bazel-skylib.git",
        "--real-from", "HEAD~1",
        "--real-to", "HEAD",
        *tail,
        "--metrics-out", "serve-stress-real-small-metrics.json",
        "--summary-out", "serve-stress-real-small-summary.md",
    ]


class FoldFlagValuesTest(unittest.TestCase):
    def test_folds_flag_shaped_value(self):
        self.assertEqual(
            fold_flag_values(["--real-clone-args", "--depth=50"]),
            ["--real-clone-args=--depth=50"],
        )

    def test_folded_value_is_equivalent_for_non_flag_values(self):
        self.assertEqual(fold_flag_values(["--real-clone-args", "x"]), ["--real-clone-args=x"])

    def test_leaves_other_options_untouched(self):
        argv = ["--real-from", "HEAD~1", "--bazel", "bazelisk", "-v"]
        self.assertEqual(fold_flag_values(argv), argv)

    def test_already_folded_value_is_untouched(self):
        argv = ["--real-clone-args=--depth=50"]
        self.assertEqual(fold_flag_values(argv), argv)

    def test_trailing_option_without_a_value_is_untouched(self):
        # Nothing to fold onto: leave it for argparse to report as missing rather than swallowing
        # whatever follows (here, nothing).
        self.assertEqual(fold_flag_values(["--real-clone-args"]), ["--real-clone-args"])


class ParseArgsTest(unittest.TestCase):
    def test_ci_argv_parses_in_both_spellings(self):
        # The regression: the space-separated spelling used to die with "expected one argument".
        for equals_spelling in (True, False):
            with self.subTest(equals_spelling=equals_spelling):
                args = parse_args(ci_argv("--depth=50", equals_spelling))
                self.assertEqual(args.real_clone_args, "--depth=50")
                self.assertEqual(args.real_repo_url,
                                 "https://github.com/bazelbuild/bazel-skylib.git")
                self.assertEqual(args.real_from, "HEAD~1")
                self.assertEqual(args.real_to, "HEAD")

    def test_clone_args_split_into_git_argv(self):
        # setup_real_clone splits the value into `git clone` args; multiple flags must survive.
        args = parse_args(["--real-clone-args", "--depth=50 --filter=blob:none"])
        self.assertEqual(args.real_clone_args.split(), ["--depth=50", "--filter=blob:none"])

    def test_empty_clone_args_means_full_clone(self):
        # An unset matrix row expands to `--real-clone-args=`, which must mean "no extra git args".
        self.assertEqual(parse_args(["--real-clone-args="]).real_clone_args, "")
        self.assertEqual(parse_args([]).real_clone_args, "")

    def test_missing_clone_args_value_still_errors(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            parse_args(["--real-clone-args"])

    def test_hermetic_defaults(self):
        # No --real-repo-url: real-repo mode stays off and the hermetic phases run.
        args = parse_args([])
        self.assertEqual(args.real_repo_url, "")
        self.assertEqual(args.bazel, "bazel")
        self.assertFalse(args.skip_build)

    def test_parser_builds(self):
        self.assertIsNotNone(build_parser())


if __name__ == "__main__":
    unittest.main()
