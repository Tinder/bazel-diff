package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.doesNotContain
import assertk.assertions.hasSize
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
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
import org.mockito.kotlin.argumentCaptor
import org.mockito.kotlin.eq
import org.mockito.kotlin.mock
import org.mockito.kotlin.never
import org.mockito.kotlin.verify
import org.mockito.kotlin.whenever

/**
 * Tests for the module-change query path in [CalculateImpactedTargetsInteractor] that
 * exercise the predicate matching changed modules to canonical external repos in
 * `allTargets`. Lives in its own test class (instead of extending
 * `CalculateImpactedTargetsInteractorTest`) so we can register a mocked
 * [BazelQueryService] in Koin without disturbing the existing global `KoinTestRule`.
 */
class CalculateImpactedTargetsInteractorModuleQueryTest : KoinTest {

  private val queryService: BazelQueryService = mock()

  @Before
  fun setUp() {
    startKoin {
      modules(
          module {
            single<Logger> { SilentLogger }
            single { queryService }
            single { GsonBuilder().disableHtmlEscaping().create() }
          })
    }
  }

  @After
  fun tearDown() {
    stopKoin()
  }

  @Test
  fun rdepsEmptyAndIdenticalHashesProducesEmptyImpactedSet() {
    runBlocking {
      // F2 sentinel for the execute() path: when rdeps returns no workspace targets
      // and from == to, the impacted set must be empty. Catches a regression where
      // the hash-diff union below the rdeps catch widens to allTargets.keys (the
      // pre-fix `computeSimpleImpactedTargets(emptyMap(), allTargets)` form).
      // Predicate shape, fan-out, and over-match cases are pinned by
      // CalculateImpactedTargetsInteractorIssue335Test and
      // CalculateImpactedTargetsInteractorTest.testUnionsRdepsAcrossChangedModules.
      val hashes = mapOf(
          "//app:app" to TargetHash("Rule", "a", "a"),
          "@@aspect_bazel_lib+//lib:foo" to TargetHash("Rule", "x", "x"))

      whenever(queryService.query(any(), any())).thenReturn(emptyList())

      val writer = StringWriter()
      CalculateImpactedTargetsInteractor().execute(
          from = hashes,
          to = hashes,
          outputWriter = writer,
          targetTypes = null,
          fromModuleGraphJson = graph("aspect_bazel_lib", "2.10.0"),
          toModuleGraphJson = graph("aspect_bazel_lib", "2.11.0"))

      assertThat(outputLines(writer)).isEmpty()
    }
  }

  @Test
  fun doesNotMatchUnrelatedRepoBySubstring() { runBlocking {
    // Module "cpp" must not match canonical "abseil-cpp+".
    val from = mapOf(
        "//app:app" to TargetHash("Rule", "a1", "a1"),
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "b", "b"),
        "@@cpp+//pkg:lib" to TargetHash("Rule", "c", "c"))
    val to = mapOf(
        "//app:app" to TargetHash("Rule", "a2", "a2"), // hash change
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "b", "b"),
        "@@cpp+//pkg:lib" to TargetHash("Rule", "c", "c"))

    whenever(queryService.query(any(), any())).thenAnswer { inv ->
      if (inv.getArgument<String>(0) == "rdeps(//..., @@cpp+//...)") {
        listOf(mockRuleTarget("//foo:bar"))
      } else emptyList<BazelTarget>()
    }

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = from,
        to = to,
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("cpp", "1.0"),
        toModuleGraphJson = graph("cpp", "2.0"))

    val queryCaptor = argumentCaptor<String>()
    verify(queryService).query(queryCaptor.capture(), any())
    assertThat(queryCaptor.allValues).hasSize(1)
    assertThat(queryCaptor.allValues).contains("rdeps(//..., @@cpp+//...)")
    assertThat(queryCaptor.allValues).doesNotContain("rdeps(//..., @@abseil-cpp+//...)")
    assertThat(outputLines(writer)).containsExactlyInAnyOrder("//foo:bar", "//app:app")
  } }

  @Test
  fun moduleWithNoMaterialisedReposIsNotQueried() { runBlocking {
    // `ghost_module` has no canonical prefix anywhere in allTargets — no
    // rdeps subprocess should spawn for this module. The early-return in
    // queryTargetsDependingOnModules' "no module repos matched" branch must
    // delegate to computeSimpleImpactedTargets(from, allTargets) so a real
    // hash diff still surfaces, not collapse to empty or to allTargets.keys.
    // The fixture has both a hash-changed and an unchanged target so the
    // assertion distinguishes "delegates correctly" from "leaks all keys".
    val from = mapOf(
        "//app:changed" to TargetHash("Rule", "a", "a"),
        "//app:unchanged" to TargetHash("Rule", "b", "b"))
    val to = mapOf(
        "//app:changed" to TargetHash("Rule", "a-new", "a-new"),
        "//app:unchanged" to TargetHash("Rule", "b", "b"))

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = from,
        to = to,
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("ghost_module", "1.0"),
        toModuleGraphJson = graph("ghost_module", "2.0"))

    verify(queryService, never()).query(any(), any())
    assertThat(outputLines(writer)).containsExactlyInAnyOrder("//app:changed")
  } }

  @Test
  fun versionBumpReportedAsAddPlusRemoveDedupesCanonical() { runBlocking {
    // findChangedModules reports a version bump as {old removed, new added}.
    // Both Modules share the canonical prefix; the union must contain it once.
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@foo+//pkg:lib" to TargetHash("Rule", "x", "x"))

    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("foo", "1.0"),
        toModuleGraphJson = graph("foo", "2.0"))

    val queryCaptor = argumentCaptor<String>()
    verify(queryService).query(queryCaptor.capture(), eq(false))
    val unioned = queryCaptor.firstValue
    assertThat(unioned.split("@@foo+//...")).hasSize(2)  // exactly one occurrence
  } }

  @Test
  fun queryFailureOnBzlmodOnlyShapeEmitsAllHashedLabels() { runBlocking {
    // No workspace-local `//...` labels, so the fallback must surface the
    // full hash set. Otherwise the downstream `excludeExternalTargets`
    // strip reduces it to empty.
    val hashes = mapOf(
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "a", "a"),
        "@@abseil-cpp+//absl:base" to TargetHash("Rule", "b", "b"),
        "//external:com_google_absl" to TargetHash("Rule", "c", "c"))

    whenever(queryService.query(any(), any()))
        .thenThrow(RuntimeException("simulated bazel query failure"))

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("abseil-cpp", "20240116.2"),
        toModuleGraphJson = graph("abseil-cpp", "20240722.0"))

    verify(queryService).query(eq("rdeps(//..., @@abseil-cpp+//...)"), eq(false))
    assertThat(outputLines(writer)).containsExactlyInAnyOrder(
        "@@abseil-cpp+//absl:strings",
        "@@abseil-cpp+//absl:base",
        "//external:com_google_absl")
  } }

  @Test
  fun queryFailureOnMixedWorkspacePreservesGranularity() { runBlocking {
    // Fallback must return only the buildable `//...` subset when one
    // exists, not every hashed label.
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "//lib:util" to TargetHash("Rule", "b", "b"),
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "c", "c"),
        "@@other+//x:y" to TargetHash("Rule", "d", "d"),
        "//external:abseil-cpp" to TargetHash("Rule", "e", "e"))

    whenever(queryService.query(any(), any()))
        .thenThrow(RuntimeException("simulated bazel query failure"))

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("abseil-cpp", "20240116.2"),
        toModuleGraphJson = graph("abseil-cpp", "20240722.0"))

    assertThat(outputLines(writer)).containsExactlyInAnyOrder(
        "//app:app", "//lib:util")
  } }

  @Test
  fun catchFallbackOnMixedWorkspaceStripsExternalUnderExcludeFlag() { runBlocking {
    // Catch-fallback's buildable subset is `!@@ && !//external:`. Under
    // excludeExternalTargets=true the downstream filter further strips
    // `//external:*`. The externally-visible contract here is that
    // //external:* never reaches the user when the flag is set, regardless
    // of which layer did the strip — so a future regression that widens
    // isBuildableWorkspaceTarget to include //external:* would still be
    // caught at the output.
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "c", "c"),
        "//external:abseil-cpp" to TargetHash("Rule", "e", "e"))

    whenever(queryService.query(any(), any()))
        .thenThrow(RuntimeException("simulated bazel query failure"))

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("abseil-cpp", "20240116.2"),
        toModuleGraphJson = graph("abseil-cpp", "20240722.0"),
        excludeExternalTargets = true)

    assertThat(outputLines(writer)).containsExactlyInAnyOrder("//app:app")
  } }

  @Test
  fun catchFallbackOnBzlmodOnlyKeepsCanonicalsUnderExcludeFlag() { runBlocking {
    // Catch-fallback on a bzlmod-only workspace emits every hashed label
    // (the buildable subset is empty). Under excludeExternalTargets=true the
    // //external:* bridges are stripped but @@canonical labels survive — the
    // headline "rebuild everything" signal still reaches the user. Catches a
    // future regression that widens excludeExternalTargets to also strip @@,
    // which would silently empty the output on bzlmod-only catch-fallbacks.
    val hashes = mapOf(
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "a", "a"),
        "@@abseil-cpp+//absl:base" to TargetHash("Rule", "b", "b"),
        "//external:com_google_absl" to TargetHash("Rule", "c", "c"))

    whenever(queryService.query(any(), any()))
        .thenThrow(RuntimeException("simulated bazel query failure"))

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("abseil-cpp", "20240116.2"),
        toModuleGraphJson = graph("abseil-cpp", "20240722.0"),
        excludeExternalTargets = true)

    assertThat(outputLines(writer)).containsExactlyInAnyOrder(
        "@@abseil-cpp+//absl:strings",
        "@@abseil-cpp+//absl:base")
  } }

  @Test
  fun executeWithDistancesRunsModuleQueryPath() { runBlocking {
    // Covers the module-query call site in `executeWithDistances`.
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@aspect_bazel_lib+//lib:foo" to TargetHash("Rule", "x", "x"))

    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().executeWithDistances(
        from = hashes,
        to = hashes,
        depEdges = emptyMap(),
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("aspect_bazel_lib", "2.10.0"),
        toModuleGraphJson = graph("aspect_bazel_lib", "2.11.0"))

    verify(queryService).query(eq("rdeps(//..., @@aspect_bazel_lib+//...)"), eq(false))
    assertThat(writer.toString()).isEqualTo("[]")
  } }

  private fun outputLines(writer: StringWriter): List<String> =
      writer.toString().lineSequence().filter { it.isNotBlank() }.toList()

  private fun mockRuleTarget(name: String): BazelTarget.Rule {
    val target = mock<BazelTarget.Rule>()
    whenever(target.name).thenReturn(name)
    return target
  }

  private fun graph(name: String, version: String): String = """
    {
      "key": "root",
      "name": "root",
      "version": "",
      "apparentName": "root",
      "dependencies": [
        {"key": "$name@$version", "name": "$name", "version": "$version", "apparentName": "$name"}
      ]
    }
  """.trimIndent()
}
