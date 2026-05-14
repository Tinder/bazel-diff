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
  fun matchesCanonicalPlusFormRepo() { runBlocking {
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@aspect_bazel_lib+//lib:foo" to TargetHash("Rule", "x", "x"),
        "@@other+//x:y" to TargetHash("Rule", "o", "o"))

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
    // Sentinel: with rdeps empty and from == to, the union must be empty too.
    // Catches a regression where the hash-diff union widens to allTargets.keys.
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
  fun matchesExtensionCreatedRepo() { runBlocking {
    // Extension-created repos use canonical forms `name++ext+repo` /
    // `name~~ext~repo`. Both are subsumed by the `name+`/`name~` prefixes.
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@rules_jvm_external+//lib:foo" to TargetHash("Rule", "x", "x"),
        "@@rules_jvm_external++maven+maven//:guava" to TargetHash("Rule", "y", "y"))

    whenever(queryService.query(any(), any())).thenReturn(emptyList())

    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = StringWriter(),
        targetTypes = null,
        fromModuleGraphJson = graph("rules_jvm_external", "5.0"),
        toModuleGraphJson = graph("rules_jvm_external", "6.0"))

    val queryCaptor = argumentCaptor<String>()
    verify(queryService).query(queryCaptor.capture(), eq(false))
    val unioned = queryCaptor.firstValue
    assertThat(unioned).contains("@@rules_jvm_external+//...")
    assertThat(unioned).contains("@@rules_jvm_external++maven+maven//...")
  } }

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
  fun transitiveModuleMatchedByNamePrefix() { runBlocking {
    val hashes = mapOf(
        "//app:app" to TargetHash("Rule", "a", "a"),
        "@@deep_transitive_dep~3.2.1//:lib" to TargetHash("Rule", "b", "b"))

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
  fun moduleWithNoMaterialisedReposIsNotQueried() { runBlocking {
    // `ghost_module` has no canonical prefix anywhere in allTargets — no
    // rdeps subprocess should spawn for this module.
    val hashes = mapOf("//app:app" to TargetHash("Rule", "a", "a"))

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
    verify(queryService).query(queryCaptor.capture(), any())
    val unioned = queryCaptor.firstValue
    assertThat(unioned).contains("@@abseil-cpp+//...")
    assertThat(unioned).contains("@@aspect_bazel_lib+//...")
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
