"""Unit tests for tools/bcr_incompatible_flags.py.

Run via Bazel:
    bazel test //tools:bcr_incompatible_flags_test
"""

import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO

from bcr_incompatible_flags import flags_for, main, parse_incompatible_flags

# Shaped like bazel-central-registry/incompatible_flags.yml on 2026-10-30,
# including a commented-out entry and a comment after a version.
SAMPLE = """\
incompatible_flags:
  # https://github.com/bazelbuild/bazel/issues/12933
  "--incompatible_config_setting_private_default_visibility":
    - 6.x
    - 7.x
    - 8.x
  # https://github.com/bazelbuild/bazel/issues/23043
  "--incompatible_autoload_externally=":
    - 7.x
    - 8.x
  # This flag will be flipped to false
  # TODO: enable this after fixing https://github.com/bazelbuild/bazel/issues/23144
  # "--noincompatible_enable_deprecated_label_apis":
  #   - 7.x
  #   - 8.x
  "--incompatible_disable_autoloads_in_main_repo":
    - last_green
    - rolling  # trailing comment
  --incompatible_strict_action_env:
    - 8.x
"""


class ParseTest(unittest.TestCase):
    def test_parses_flags_in_order_with_versions(self):
        flags = parse_incompatible_flags(SAMPLE)
        self.assertEqual(
            list(flags),
            [
                "--incompatible_config_setting_private_default_visibility",
                "--incompatible_autoload_externally=",
                "--incompatible_disable_autoloads_in_main_repo",
                "--incompatible_strict_action_env",
            ],
        )
        self.assertEqual(flags["--incompatible_autoload_externally="], ["7.x", "8.x"])
        self.assertEqual(
            flags["--incompatible_disable_autoloads_in_main_repo"], ["last_green", "rolling"]
        )

    def test_commented_out_entry_is_skipped(self):
        self.assertNotIn(
            "--noincompatible_enable_deprecated_label_apis", parse_incompatible_flags(SAMPLE)
        )

    def test_flag_without_versions_is_kept_but_matches_nothing(self):
        flags = parse_incompatible_flags('incompatible_flags:\n  "--incompatible_x":\n')
        self.assertEqual(flags, {"--incompatible_x": []})
        self.assertEqual(flags_for("8.x", flags), [])

    def test_empty_mapping(self):
        self.assertEqual(parse_incompatible_flags("incompatible_flags:\n"), {})

    def test_missing_root_is_an_error(self):
        with self.assertRaises(ValueError):
            parse_incompatible_flags('"--incompatible_x":\n  - 8.x\n')

    def test_version_before_flag_is_an_error(self):
        with self.assertRaises(ValueError):
            parse_incompatible_flags("incompatible_flags:\n  - 8.x\n")

    def test_unrecognized_line_is_an_error(self):
        with self.assertRaises(ValueError):
            parse_incompatible_flags("incompatible_flags:\n  nested:\n    deeper: 1\n")


class FlagsForTest(unittest.TestCase):
    def test_selects_by_version(self):
        flags = parse_incompatible_flags(SAMPLE)
        self.assertEqual(
            flags_for("8.x", flags),
            [
                "--incompatible_config_setting_private_default_visibility",
                "--incompatible_autoload_externally=",
                "--incompatible_strict_action_env",
            ],
        )
        self.assertEqual(flags_for("6.x", flags), ["--incompatible_config_setting_private_default_visibility"])
        self.assertEqual(flags_for("last_green", flags), ["--incompatible_disable_autoloads_in_main_repo"])

    def test_version_with_no_flags(self):
        self.assertEqual(flags_for("9.x", parse_incompatible_flags(SAMPLE)), [])


class MainTest(unittest.TestCase):
    def _run(self, text, version):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "incompatible_flags.yml")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(text)
            out, err = StringIO(), StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                code = main(["--file", path, version])
        return code, out.getvalue(), err.getvalue()

    def test_prints_one_flag_per_line(self):
        code, out, err = self._run(SAMPLE, "8.x")
        self.assertEqual(code, 0)
        self.assertEqual(
            out.splitlines(),
            [
                "--incompatible_config_setting_private_default_visibility",
                "--incompatible_autoload_externally=",
                "--incompatible_strict_action_env",
            ],
        )
        self.assertEqual(err, "")

    def test_no_flags_prints_nothing_and_succeeds(self):
        code, out, _ = self._run(SAMPLE, "9.x")
        self.assertEqual(code, 0)
        self.assertEqual(out, "")

    def test_unparsable_file_fails(self):
        code, out, err = self._run("not: [the, shape]\n", "8.x")
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertIn("bcr_incompatible_flags:", err)

    def test_missing_file_fails(self):
        out, err = StringIO(), StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = main(["--file", "/nonexistent/incompatible_flags.yml", "8.x"])
        self.assertEqual(code, 1)
        self.assertIn("bcr_incompatible_flags:", err.getvalue())


if __name__ == "__main__":
    unittest.main()
