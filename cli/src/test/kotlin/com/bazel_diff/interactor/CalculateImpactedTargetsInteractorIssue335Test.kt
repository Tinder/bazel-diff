package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.doesNotContain
import assertk.assertions.hasSize
import com.bazel_diff.SilentLogger
import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.bazel.BazelTarget
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.log.Logger
import com.google.gson.GsonBuilder
import java.io.StringWriter
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.mockito.kotlin.any
import org.mockito.kotlin.eq
import org.mockito.kotlin.mock
import org.mockito.kotlin.whenever

/**
 * Reproducer for the "loose substring match" tail of
 * https://github.com/Tinder/bazel-diff/issues/335 (fix #3 in the issue body).
 *
 * `CalculateImpactedTargetsInteractor.queryTargetsDependingOnModules` resolves changed module
 * keys to canonical external repos with:
 *
 * ```
 * val moduleRepos = allTargets.keys
 *     .filter { it.startsWith("@@") && it.contains(moduleName) }
 *     .map { it.substring(2).substringBefore("//") }
 *     .toSet()
 * ```
 *
 * `it.contains(moduleName)` is a very loose match. In a workspace with many module extensions
 * a single changed module like `aspect_bazel_lib` matches every canonical repo whose name
 * starts with that string -- e.g. `@@aspect_bazel_lib+`, `@@aspect_bazel_lib++toolchains+bats_toolchains`,
 * etc. Each match becomes its own serial `bazel query rdeps(//..., @@<repo>//...)` subprocess.
 * On the workspace in #335 that fan-out produced ~5,000 subprocesses and took multiple hours.
 *
 * This test asserts that resolving a single changed module yields at most one rdeps query
 * (the one targeting the actual changed repo). The fix is either to match the canonical repo
 * key exactly (e.g. `aspect_bazel_lib+` -- the part before any further `+`/`~`), or to look up
 * the repo via `bazel mod dump_repo_mapping` instead of a substring scan.
 */
class CalculateImpactedTargetsInteractorIssue335Test : KoinTest {

  private val queryService: BazelQueryService = mock()
  private val capturedQueries = mutableListOf<String>()

  @Before
  fun setUp() {
    // Capture every query expression that flows through queryService.query(...).
    runBlocking {
      whenever(queryService.query(any<String>(), eq(false))).thenAnswer { invocation ->
        val q = invocation.getArgument<String>(0)
        capturedQueries.add(q)
        emptyList<BazelTarget>()
      }
      whenever(queryService.query(any<String>())).thenAnswer { invocation ->
        val q = invocation.getArgument<String>(0)
        capturedQueries.add(q)
        emptyList<BazelTarget>()
      }
    }
    startKoin {
      modules(
          module {
            single<Logger> { SilentLogger }
            single { queryService }
            single { GsonBuilder().disableHtmlEscaping().setPrettyPrinting().create() }
          })
    }
  }

  @After
  fun tearDown() {
    stopKoin()
  }

  @Test
  @org.junit.Ignore(
      "Reproducer for https://github.com/Tinder/bazel-diff/issues/335 fix #3 (loose " +
          "substring match in queryTargetsDependingOnModules). Today a single changed module " +
          "fans out into one rdeps subprocess per repo whose canonical name contains the " +
          "apparent module name, which explodes on workspaces with many module extensions. " +
          "Drop @Ignore once the match is tightened to the actual canonical repo key.")
  fun queryTargetsDependingOnModules_doesNotOverMatchExtensionRepoNames_reproducerForIssue335() {
    // Simulate the #335 workspace: one module (aspect_bazel_lib) and several module-extension
    // repos whose canonical names also begin with "aspect_bazel_lib". The bug is that all
    // of those get treated as the "changed module" and each spawns its own rdeps query.
    val startHashes =
        mapOf(
            "//:target1" to TargetHash("Rule", "h1", "h1"),
            "@@aspect_bazel_lib+//foo:bar" to TargetHash("Rule", "h2-v1", "h2-v1"),
            "@@aspect_bazel_lib++toolchains+bats_toolchains//x:y" to
                TargetHash("Rule", "h3", "h3"),
            "@@aspect_bazel_lib++toolchains+yq_toolchains//x:y" to
                TargetHash("Rule", "h4", "h4"),
            "@@unrelated_repo+//pkg:tgt" to TargetHash("Rule", "h5", "h5"),
        )
    // Only the aspect_bazel_lib *module* itself changes version; the extension repos do not
    // appear in the module graph at all.
    val endHashes = startHashes.toMutableMap()
    endHashes["@@aspect_bazel_lib+//foo:bar"] = TargetHash("Rule", "h2-v2", "h2-v2")

    val fromModuleGraph =
        """
        {
          "key": "root",
          "name": "root",
          "version": "",
          "apparentName": "root",
          "dependencies": [
            {"key": "aspect_bazel_lib@1.0.0", "name": "aspect_bazel_lib", "version": "1.0.0", "apparentName": "aspect_bazel_lib"}
          ]
        }
        """
            .trimIndent()
    val toModuleGraph =
        """
        {
          "key": "root",
          "name": "root",
          "version": "",
          "apparentName": "root",
          "dependencies": [
            {"key": "aspect_bazel_lib@2.0.0", "name": "aspect_bazel_lib", "version": "2.0.0", "apparentName": "aspect_bazel_lib"}
          ]
        }
        """
            .trimIndent()

    val outputWriter = StringWriter()
    CalculateImpactedTargetsInteractor()
        .execute(
            from = startHashes,
            to = endHashes,
            outputWriter = outputWriter,
            targetTypes = null,
            fromModuleGraphJson = fromModuleGraph,
            toModuleGraphJson = toModuleGraph,
        )

    val rdepsQueries = capturedQueries.filter { it.contains("rdeps(") }
    // Desired: exactly ONE rdeps query, scoped to the actual changed repo.
    // Today: one query per canonical repo containing the substring "aspect_bazel_lib", so
    // this list has 3 entries -- aspect_bazel_lib+, aspect_bazel_lib++toolchains+bats_toolchains,
    // and aspect_bazel_lib++toolchains+yq_toolchains.
    assertThat(rdepsQueries).hasSize(1)
    assertThat(rdepsQueries[0]).contains("@@aspect_bazel_lib+//...")
    assertThat(rdepsQueries[0]).doesNotContain("toolchains+bats_toolchains")
    assertThat(rdepsQueries[0]).doesNotContain("toolchains+yq_toolchains")
  }
}
