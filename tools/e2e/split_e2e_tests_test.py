"""Unit tests for the e2e test splitter.

Every case here drives the parsers with an inline source snippet rather than the
repo's own e2e files, so the tests keep asserting the same thing as those files
change -- and so they can reach the failures a run against the real sources
never does (a duplicate target name, a `#[test]` in an undeclared module, an
unknown timeout).
"""

import pathlib
import shutil
import tempfile
import textwrap
import unittest

import split_e2e_tests as splitter


class RustParsingTest(unittest.TestCase):
    def parse(self, source, prefix="core"):
        return splitter.parse_rust_source(textwrap.dedent(source), prefix)

    def test_collects_test_functions_under_the_module_prefix(self):
        cases = self.parse(
            """
            #[test]
            fn integration_golden() {}

            fn helper(arg: &str) {}

            #[test]
            fn integration_no_keep_going() {}
            """
        )

        self.assertEqual(
            ["core::integration_golden", "core::integration_no_keep_going"],
            [case.name for case in cases],
        )

    def test_attributes_between_test_and_fn_are_skipped(self):
        cases = self.parse(
            """
            #[test]
            #[ignore = "fixture pins Bazel 7"]
            fn bzlmod_cc_transitive_deps_query() {}
            """
        )

        self.assertEqual(["core::bzlmod_cc_transitive_deps_query"], [case.name for case in cases])

    def test_inline_modules_nest_the_path(self):
        cases = self.parse(
            """
            #[test]
            fn top() {}

            mod inner {
                #[test]
                fn nested() {}

                mod deeper {
                    #[test]
                    fn deepest() {}
                }
            }

            #[test]
            fn after_the_module() {}
            """
        )

        self.assertEqual(
            [
                "core::top",
                "core::inner::nested",
                "core::inner::deeper::deepest",
                "core::after_the_module",
            ],
            [case.name for case in cases],
        )

    def test_marker_comment_overrides_the_timeout(self):
        cases = self.parse(
            """
            // e2e-timeout: long
            #[test]
            fn slow_case() {}

            // e2e-timeout: short
            #[test]
            fn quick_case() {}

            #[test]
            fn default_case() {}
            """
        )

        self.assertEqual(
            [
                ("core::slow_case", "long"),
                ("core::quick_case", "short"),
                ("core::default_case", "moderate"),
            ],
            [(case.name, case.timeout) for case in cases],
        )

    def test_braces_inside_strings_and_comments_do_not_desync_modules(self):
        cases = self.parse(
            """
            mod inner {
                #[test]
                fn nested() {
                    let noise = "a { brace } in a string";
                    // and a } in a comment
                    assert!(noise.contains('{'));
                }
            }

            #[test]
            fn after_the_module() {}
            """
        )

        self.assertEqual(
            ["core::inner::nested", "core::after_the_module"], [case.name for case in cases]
        )

    def test_empty_prefix_leaves_the_name_unqualified(self):
        cases = self.parse(
            """
            #[test]
            fn root_level() {}
            """,
            prefix="",
        )

        self.assertEqual(["root_level"], [case.name for case in cases])


class RustModulePathTest(unittest.TestCase):
    def test_module_prefix_derivation(self):
        self.assertEqual("core", splitter.rust_module_prefix("tests/e2e/core.rs"))
        self.assertEqual("support", splitter.rust_module_prefix("tests/e2e/support/mod.rs"))
        self.assertEqual("support::util", splitter.rust_module_prefix("tests/e2e/support/util.rs"))

    def test_declared_modules(self):
        declared = splitter.rust_declared_modules(
            textwrap.dedent(
                """
                #[path = "e2e/core.rs"]
                mod core;
                #[path = "e2e/support/mod.rs"]
                mod support;
                """
            )
        )

        self.assertEqual({"core", "support"}, declared)


class CollectionTest(unittest.TestCase):
    """Drives the collectors over a throwaway tree laid out like the real repo."""

    def build_repo(self, rust_crate_root, rust_modules):
        root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root, True)

        rust_dir = root / splitter.RUST_E2E_DIR
        rust_dir.mkdir(parents=True)
        (root / splitter.RUST_CRATE_ROOT).write_text(rust_crate_root)
        for name, body in rust_modules.items():
            (rust_dir / name).write_text(body)
        return root

    def test_collects_cases_under_the_module_prefix(self):
        root = self.build_repo(
            '#[path = "e2e/core.rs"]\nmod core;\n',
            {"core.rs": "#[test]\nfn alpha() {}\n"},
        )

        self.assertEqual(
            ["core::alpha"], [case.name for case in splitter.collect_rust_suites(root)[0].cases]
        )

    def test_undeclared_rust_module_is_rejected(self):
        # Rust would silently never compile the file; without this check the
        # regen would silently never generate a target for it either.
        root = self.build_repo(
            '#[path = "e2e/core.rs"]\nmod core;\n',
            {
                "core.rs": "#[test]\nfn alpha() {}\n",
                "orphan.rs": "#[test]\nfn beta() {}\n",
            },
        )

        with self.assertRaisesRegex(splitter.GeneratorError, "mod orphan;` is missing"):
            splitter.collect_rust_suites(root)

    def test_helper_module_without_tests_needs_no_mod_line(self):
        root = self.build_repo(
            '#[path = "e2e/core.rs"]\nmod core;\n',
            {
                "core.rs": "#[test]\nfn alpha() {}\n",
                "fixtures.rs": "pub fn helper() {}\n",
            },
        )

        self.assertEqual(
            ["core::alpha"], [case.name for case in splitter.collect_rust_suites(root)[0].cases]
        )


class TargetNameTest(unittest.TestCase):
    def test_rust_target_name_flattens_the_module_path(self):
        self.assertEqual(
            "e2e_test_core_integration_golden",
            splitter.rust_target_name("e2e_test", splitter.Case(name="core::integration_golden")),
        )

    def test_colliding_target_names_are_rejected(self):
        # `core::a_b` and `core_a::b` both flatten to e2e_test_core_a_b. Bazel
        # would report the duplicate as a cryptic redefinition inside a macro.
        with self.assertRaisesRegex(splitter.GeneratorError, "both want the Bazel target name"):
            splitter._assert_unique_targets(["e2e_test_core_a_b", "e2e_test_core_a_b"])


class RenderingTest(unittest.TestCase):
    def test_rust_render_is_loadable_starlark_shaped_python(self):
        rendered = splitter.render_rust_bzl(
            [splitter.RustSuite(name="e2e_test", cases=[splitter.Case(name="core::alpha")])]
        )

        namespace = {}
        exec(compile(rendered, "rust_e2e_cases.bzl", "exec"), namespace)
        self.assertEqual(
            [{"name": "e2e_test", "cases": [{"name": "core::alpha", "timeout": "moderate"}]}],
            namespace["RUST_E2E_SUITES"],
        )

    def test_render_is_marked_generated(self):
        rendered = splitter.render_rust_bzl([splitter.RustSuite(name="e2e_test")])
        self.assertTrue(rendered.startswith(splitter.GENERATED_HEADER))
        self.assertTrue(rendered.endswith("\n"))


class RepoConsistencyTest(unittest.TestCase):
    """Guards the invariants the generated output has to hold for Bazel."""

    def test_every_declared_timeout_is_a_bazel_timeout(self):
        repo_root = splitter.find_repo_root()
        cases = splitter.collect_rust_suites(repo_root)[0].cases

        self.assertTrue(cases, "the repo should have e2e cases to split")
        for case in cases:
            self.assertIn(case.timeout, splitter.VALID_TIMEOUTS, case.name)

    def test_generation_is_deterministic(self):
        repo_root = splitter.find_repo_root()
        self.assertEqual(splitter.generate(repo_root), splitter.generate(repo_root))


if __name__ == "__main__":
    unittest.main()
