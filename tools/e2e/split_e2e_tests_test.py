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


class KotlinParsingTest(unittest.TestCase):
    def parse(self, source, path="cli/src/test/kotlin/com/bazel_diff/e2e/Sample.kt"):
        return splitter.parse_kotlin_source(textwrap.dedent(source), path)

    def test_collects_test_methods_and_qualifies_the_class(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class SampleTest {
              @Test
              fun testAlpha() {}

              @Test
              fun testBeta() {}
            }
            """
        )

        self.assertEqual(1, len(suites))
        self.assertEqual("SampleTest", suites[0].name)
        self.assertEqual("com.bazel_diff.e2e.SampleTest", suites[0].test_class)
        self.assertEqual(["testAlpha", "testBeta"], [case.name for case in suites[0].cases])

    def test_ignores_helper_methods(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class SampleTest {
              private fun helper() {}

              @Test
              fun testAlpha() {
                helper()
              }

              private fun anotherHelper(): String = "x"
            }
            """
        )

        self.assertEqual(["testAlpha"], [case.name for case in suites[0].cases])

    def test_test_annotation_survives_intervening_annotations(self):
        # E2ETest really does have a @Test whose fun is four lines below it,
        # separated by a multi-line @org.junit.Ignore.
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class SampleTest {
              @Test
              @org.junit.Ignore(
                  "fixture pins an old Bazel layout " +
                      "that no longer loads")
              fun testSkipped() {}
            }
            """
        )

        self.assertEqual(["testSkipped"], [case.name for case in suites[0].cases])

    def test_default_timeout_is_the_sixty_second_cap(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class SampleTest {
              @Test
              fun testAlpha() {}
            }
            """
        )

        self.assertEqual("short", suites[0].cases[0].timeout)
        self.assertEqual(60, splitter.VALID_TIMEOUTS[suites[0].cases[0].timeout])

    def test_marker_comment_overrides_the_timeout(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class SampleTest {
              // e2e-timeout: moderate
              @Test
              fun testSlow() {}

              @Test
              fun testFast() {}
            }
            """
        )

        self.assertEqual(
            [("testSlow", "moderate"), ("testFast", "short")],
            [(case.name, case.timeout) for case in suites[0].cases],
        )

    def test_marker_reaches_past_a_doc_comment(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class SampleTest {
              // e2e-timeout: long
              // Downloads an Android SDK before it can assert anything.
              @Test
              fun testSlow() {}
            }
            """
        )

        self.assertEqual("long", suites[0].cases[0].timeout)

    def test_marker_does_not_leak_across_a_blank_line(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class SampleTest {
              // e2e-timeout: moderate

              @Test
              fun testAlpha() {}
            }
            """
        )

        self.assertEqual("short", suites[0].cases[0].timeout)

    def test_unknown_timeout_is_rejected(self):
        with self.assertRaisesRegex(splitter.GeneratorError, "unknown e2e-timeout"):
            self.parse(
                """
                package com.bazel_diff.e2e

                class SampleTest {
                  // e2e-timeout: quick
                  @Test
                  fun testAlpha() {}
                }
                """
            )

    def test_several_classes_in_one_file_become_several_suites(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class FirstTest {
              @Test
              fun testAlpha() {}
            }

            class SecondTest {
              @Test
              fun testBeta() {}
            }
            """
        )

        self.assertEqual(["FirstTest", "SecondTest"], [suite.name for suite in suites])
        self.assertEqual(["testBeta"], [case.name for case in suites[1].cases])

    def test_classes_without_tests_are_dropped(self):
        suites = self.parse(
            """
            package com.bazel_diff.e2e

            class Fixtures {
              fun helper() {}
            }
            """
        )

        self.assertEqual([], suites)

    def test_missing_package_is_rejected(self):
        with self.assertRaisesRegex(splitter.GeneratorError, "no package declaration"):
            self.parse(
                """
                class SampleTest {
                  @Test
                  fun testAlpha() {}
                }
                """
            )

    def test_backtick_name_is_rejected(self):
        # JUnit allows it; a Bazel target name does not, so fail loudly rather
        # than emitting a label Bazel will reject with no explanation.
        with self.assertRaisesRegex(splitter.GeneratorError, "spaces in its name"):
            self.parse(
                """
                package com.bazel_diff.e2e

                class SampleTest {
                  @Test
                  fun `impacted targets are hermetic`() {}
                }
                """
            )


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
            // e2e-timeout: moderate
            #[test]
            fn slow_case() {}

            #[test]
            fn fast_case() {}
            """
        )

        self.assertEqual(
            [("core::slow_case", "moderate"), ("core::fast_case", "short")],
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

        kotlin_dir = root / splitter.KOTLIN_E2E_DIR
        kotlin_dir.mkdir(parents=True)
        (kotlin_dir / "SampleTest.kt").write_text(
            "package com.bazel_diff.e2e\n\nclass SampleTest {\n  @Test\n  fun testAlpha() {}\n}\n"
        )

        rust_dir = root / splitter.RUST_E2E_DIR
        rust_dir.mkdir(parents=True)
        (root / splitter.RUST_CRATE_ROOT).write_text(rust_crate_root)
        for name, body in rust_modules.items():
            (rust_dir / name).write_text(body)
        return root

    def test_collects_both_languages(self):
        root = self.build_repo(
            '#[path = "e2e/core.rs"]\nmod core;\n',
            {"core.rs": "#[test]\nfn alpha() {}\n"},
        )

        kotlin = splitter.collect_kotlin_suites(root)
        self.assertEqual(["testAlpha"], [case.name for case in kotlin[0].cases])
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

    def test_case_named_all_would_shadow_the_unsplit_target(self):
        # kt_jvm_e2e_tests also emits `<Suite>_all`, so a `@Test fun all()`
        # would redefine it -- Bazel reports that as a bare "rule 'SampleTest_all'
        # is already defined", with no hint at which test caused it.
        root = self.build_repo(
            '#[path = "e2e/core.rs"]\nmod core;\n',
            {"core.rs": "#[test]\nfn alpha() {}\n"},
        )
        kotlin_dir = root / splitter.KOTLIN_E2E_DIR
        (kotlin_dir / "SampleTest.kt").write_text(
            "package com.bazel_diff.e2e\n\nclass SampleTest {\n  @Test\n  fun all() {}\n}\n"
        )

        with self.assertRaisesRegex(splitter.GeneratorError, "SampleTest_all"):
            splitter.collect_kotlin_suites(root)

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
    def test_kotlin_target_name(self):
        suite = splitter.KotlinSuite(name="E2ETest", test_class="com.bazel_diff.e2e.E2ETest")
        self.assertEqual(
            "E2ETest_testE2E", splitter.kotlin_target_name(suite, splitter.Case(name="testE2E"))
        )

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
    def test_kotlin_render_is_loadable_starlark_shaped_python(self):
        rendered = splitter.render_kotlin_bzl(
            [
                splitter.KotlinSuite(
                    name="E2ETest",
                    test_class="com.bazel_diff.e2e.E2ETest",
                    cases=[
                        splitter.Case(name="testAlpha"),
                        splitter.Case(name="testBeta", timeout="moderate"),
                    ],
                )
            ]
        )

        namespace = {}
        exec(compile(rendered, "kotlin_e2e_cases.bzl", "exec"), namespace)
        self.assertEqual(
            [
                {
                    "name": "E2ETest",
                    "test_class": "com.bazel_diff.e2e.E2ETest",
                    "cases": [
                        {"name": "testAlpha", "timeout": "short"},
                        {"name": "testBeta", "timeout": "moderate"},
                    ],
                }
            ],
            namespace["KOTLIN_E2E_SUITES"],
        )

    def test_rust_render_is_loadable_starlark_shaped_python(self):
        rendered = splitter.render_rust_bzl(
            [splitter.RustSuite(name="e2e_test", cases=[splitter.Case(name="core::alpha")])]
        )

        namespace = {}
        exec(compile(rendered, "rust_e2e_cases.bzl", "exec"), namespace)
        self.assertEqual(
            [{"name": "e2e_test", "cases": [{"name": "core::alpha", "timeout": "short"}]}],
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
        suites = splitter.collect_kotlin_suites(repo_root)
        cases = [case for suite in suites for case in suite.cases]
        cases += splitter.collect_rust_suites(repo_root)[0].cases

        self.assertTrue(cases, "the repo should have e2e cases to split")
        for case in cases:
            self.assertIn(case.timeout, splitter.VALID_TIMEOUTS, case.name)

    def test_generation_is_deterministic(self):
        repo_root = splitter.find_repo_root()
        self.assertEqual(splitter.generate(repo_root), splitter.generate(repo_root))


if __name__ == "__main__":
    unittest.main()
