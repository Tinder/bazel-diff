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
import org.mockito.kotlin.times
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
  fun matchesCanonicalPlusFormRepo() { runBlocking {
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@aspect_bazel_lib+//lib:foo" to TargetHash("Rule", "x", "x"),
        "@@other+//x:y" to TargetHash("Rule", "o", "o"))

    whenever(queryService.discoverRepoMapping())
        .thenReturn(mapOf("aspect_bazel_lib" to "aspect_bazel_lib+"))
    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    val writer = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = writer,
        targetTypes = null,
        fromModuleGraphJson = graph("aspect_bazel_lib", "2.10.0"),
        toModuleGraphJson = graph("aspect_bazel_lib", "2.11.0"))

    verify(queryService).query(eq("rdeps(//..., @@aspect_bazel_lib+//...)"), eq(false))
    verify(queryService, never()).query(eq("rdeps(//..., @@other+//...)"), eq(false))
    // rdeps returned empty and there are no hash-diff changes (from == to), so
    // the impacted set must be empty. Guards against regression to the prior
    // `computeSimpleImpactedTargets(emptyMap(), allTargets)` form, which unioned
    // the full workspace back into the output.
    assertThat(outputLines(writer)).isEmpty()
  } }

  @Test
  fun matchesCanonicalTildeFormRepo() { runBlocking {
    val start = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@abseil-cpp~20240116.2//absl:strings" to TargetHash("Rule", "x", "x"))
    val end = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@abseil-cpp~20240722.0//absl:strings" to TargetHash("Rule", "y", "y"))

    whenever(queryService.discoverRepoMapping()).thenReturn(emptyMap())
    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    CalculateImpactedTargetsInteractor().execute(
        from = start,
        to = end,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("abseil-cpp", "20240116.2"),
        toModuleGraphJson = graph("abseil-cpp", "20240722.0"))

    verify(queryService).query(eq("rdeps(//..., @@abseil-cpp~20240722.0//...)"), eq(false))
  } }

  @Test
  fun matchesExtensionCreatedRepoViaMapping() { runBlocking {
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@rules_jvm_external+//lib:foo" to TargetHash("Rule", "x", "x"),
        "@@rules_jvm_external++maven+maven//:guava" to TargetHash("Rule", "y", "y"))

    whenever(queryService.discoverRepoMapping())
        .thenReturn(mapOf("rules_jvm_external" to "rules_jvm_external+"))
    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("rules_jvm_external", "5.0"),
        toModuleGraphJson = graph("rules_jvm_external", "6.0"))

    verify(queryService).query(eq("rdeps(//..., @@rules_jvm_external+//...)"), eq(false))
    verify(queryService).query(
        eq("rdeps(//..., @@rules_jvm_external++maven+maven//...)"), eq(false))
  } }

  @Test
  fun matchesExtensionCreatedRepoViaNameFallback() { runBlocking {
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@rules_jvm_external+//lib:foo" to TargetHash("Rule", "x", "x"),
        "@@rules_jvm_external++maven+maven//:guava" to TargetHash("Rule", "y", "y"))

    whenever(queryService.discoverRepoMapping()).thenReturn(emptyMap())
    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("rules_jvm_external", "5.0"),
        toModuleGraphJson = graph("rules_jvm_external", "6.0"))

    verify(queryService).query(eq("rdeps(//..., @@rules_jvm_external+//...)"), eq(false))
    verify(queryService).query(
        eq("rdeps(//..., @@rules_jvm_external++maven+maven//...)"), eq(false))
  } }

  @Test
  fun doesNotMatchUnrelatedRepoBySubstring() { runBlocking {
    // Headline regression guard: module "cpp" must not match canonical "abseil-cpp+".
    // Also asserts on output: stub `rdeps(@@cpp+//...)` to return `//foo:bar` and add
    // a hash-diff change in `//app:app`. The impacted set must be exactly
    // {//foo:bar, //app:app} — never {@@abseil-cpp+//absl:strings}.
    val from = mapOf(
        "//app:app" to TargetHash("Rule", "a1", "a1"),
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "b", "b"),
        "@@cpp+//pkg:lib" to TargetHash("Rule", "c", "c"))
    val to = mapOf(
        "//app:app" to TargetHash("Rule", "a2", "a2"), // hash change
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "b", "b"),
        "@@cpp+//pkg:lib" to TargetHash("Rule", "c", "c"))

    whenever(queryService.discoverRepoMapping()).thenReturn(emptyMap())
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
    // Regression guard on the F2 fix: `@@abseil-cpp+//absl:strings` must NOT
    // leak into the impacted set just because it shares a canonical-repo
    // substring with the changed module.
    assertThat(outputLines(writer)).containsExactlyInAnyOrder("//foo:bar", "//app:app")
  } }

  @Test
  fun transitiveModuleNotInRootMappingUsesNameFallback() { runBlocking {
    // Root mapping does not include deep_transitive_dep; Tier B matches by name prefix.
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@deep_transitive_dep~3.2.1//:lib" to TargetHash("Rule", "b", "b"))

    whenever(queryService.discoverRepoMapping())
        .thenReturn(mapOf("some_other_module" to "some_other_module+"))
    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("deep_transitive_dep", "3.2.0"),
        toModuleGraphJson = graph("deep_transitive_dep", "3.2.1"))

    verify(queryService).query(
        eq("rdeps(//..., @@deep_transitive_dep~3.2.1//...)"), eq(false))
  } }

  @Test
  fun transitiveModuleWithNoMaterialisedReposIsNotQueried() { runBlocking {
    // `ghost_module` has no canonical prefix anywhere in allTargets. The key
    // behaviour under test is "no rdeps subprocess spawns for this module";
    // the overall impacted set is determined by the existing conservative
    // union (out of scope for this PR).
    val hashes = mapOf("//app:app" to TargetHash("Rule", "a", "a"))

    whenever(queryService.discoverRepoMapping()).thenReturn(emptyMap())

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("ghost_module", "1.0"),
        toModuleGraphJson = graph("ghost_module", "2.0"))

    verify(queryService, never()).query(any(), any())
  } }

  @Test
  fun multipleChangedModulesProduceDisjointMatchSets() { runBlocking {
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@abseil-cpp+//a:1" to TargetHash("Rule", "b", "b"),
        "@@aspect_bazel_lib+//b:2" to TargetHash("Rule", "c", "c"))

    whenever(queryService.discoverRepoMapping()).thenReturn(
        mapOf(
            "abseil-cpp" to "abseil-cpp+",
            "aspect_bazel_lib" to "aspect_bazel_lib+"))
    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    val fromGraph = """
      {
        "key": "root", "name": "root", "version": "", "apparentName": "root",
        "dependencies": [
          {"key": "abseil-cpp@1.0", "name": "abseil-cpp", "version": "1.0", "apparentName": "abseil-cpp"},
          {"key": "aspect_bazel_lib@2.0", "name": "aspect_bazel_lib", "version": "2.0", "apparentName": "aspect_bazel_lib"}
        ]
      }
    """.trimIndent()
    val toGraph = """
      {
        "key": "root", "name": "root", "version": "", "apparentName": "root",
        "dependencies": [
          {"key": "abseil-cpp@2.0", "name": "abseil-cpp", "version": "2.0", "apparentName": "abseil-cpp"},
          {"key": "aspect_bazel_lib@3.0", "name": "aspect_bazel_lib", "version": "3.0", "apparentName": "aspect_bazel_lib"}
        ]
      }
    """.trimIndent()

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = fromGraph,
        toModuleGraphJson = toGraph)

    val queryCaptor = argumentCaptor<String>()
    verify(queryService, times(2)).query(queryCaptor.capture(), any())
    assertThat(queryCaptor.allValues).hasSize(2)
    assertThat(queryCaptor.allValues).contains("rdeps(//..., @@abseil-cpp+//...)")
    assertThat(queryCaptor.allValues).contains("rdeps(//..., @@aspect_bazel_lib+//...)")
  } }

  @Test
  fun discoverRepoMappingFailureFallsBackToNameTier() { runBlocking {
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@abseil-cpp+//absl:strings" to TargetHash("Rule", "b", "b"))

    whenever(queryService.discoverRepoMapping())
        .thenThrow(RuntimeException("simulated mapping failure"))
    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("abseil-cpp", "20240116.2"),
        toModuleGraphJson = graph("abseil-cpp", "20240722.0"))

    verify(queryService).query(eq("rdeps(//..., @@abseil-cpp+//...)"), eq(false))
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

    whenever(queryService.discoverRepoMapping())
        .thenReturn(mapOf("abseil-cpp" to "abseil-cpp+"))
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

    whenever(queryService.discoverRepoMapping())
        .thenReturn(mapOf("abseil-cpp" to "abseil-cpp+"))
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
  fun executeWithDistancesRunsModuleQueryPath() { runBlocking {
    // Covers the parallel module-query call site in `executeWithDistances`.
    // Also confirms the F2 fix holds for the distances-output path: when `from == to`
    // and rdeps returns empty, the JSON output is the empty-array `[]`, not every label.
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@aspect_bazel_lib+//lib:foo" to TargetHash("Rule", "x", "x"))

    whenever(queryService.discoverRepoMapping())
        .thenReturn(mapOf("aspect_bazel_lib" to "aspect_bazel_lib+"))
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
