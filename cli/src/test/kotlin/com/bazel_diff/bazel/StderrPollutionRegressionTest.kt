package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.hasSize
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor
import com.bazel_diff.testModule
import java.io.StringWriter
import org.junit.Rule
import org.junit.Test
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule

/**
 * Regression guard for the 17.0.1..18.0.5 -> 18.1.0+ stderr-pollution compatibility break
 * introduced by PR #330. See `ModuleGraphParser.parseModuleGraph` for the fix.
 */
class StderrPollutionRegressionTest : KoinTest {
  @get:Rule val koinTestRule = KoinTestRule.create { modules(testModule()) }

  private val parser = ModuleGraphParser()

  // Shape of the stderr lines 17.0.1..18.0.5 captured into `moduleGraphJson`.
  private val stderrPrefix =
      """
      Computing main repo mapping:
      Loading:
      Loading: 0 packages loaded
      Analyzing: 0 targets (0 packages loaded, 0 targets configured)
      INFO: Invocation ID: 4d8d5c62-1f1c-4f72-9a3e-5fbd5e6ac3d2
      INFO: Current date is 2026-04-20
      """.trimIndent()

  private val cleanGraphJson =
      """
      {
        "key": "<root>",
        "name": "my-workspace",
        "version": "",
        "apparentName": "my-workspace",
        "dependencies": [
          {"key": "bazel_tools@_", "name": "bazel_tools", "version": "_", "apparentName": "bazel_tools"},
          {"key": "abseil-cpp@20240116.2", "name": "abseil-cpp", "version": "20240116.2", "apparentName": "com_google_absl"},
          {"key": "aspect_bazel_lib@2.22.5", "name": "aspect_bazel_lib", "version": "2.22.5", "apparentName": "aspect_bazel_lib"},
          {"key": "rules_jvm_external@6.10", "name": "rules_jvm_external", "version": "6.10", "apparentName": "rules_jvm_external"},
          {"key": "rules_python@1.8.4", "name": "rules_python", "version": "1.8.4", "apparentName": "rules_python"},
          {"key": "googletest@1.14.0", "name": "googletest", "version": "1.14.0", "apparentName": "com_google_googletest"}
        ]
      }
      """.trimIndent()

  private val pollutedGraphJson = "$stderrPrefix\n$cleanGraphJson"

  @Test
  fun `18_0_x polluted moduleGraphJson now parses correctly`() {
    val clean = parser.parseModuleGraph(cleanGraphJson)
    val result = parser.parseModuleGraph(pollutedGraphJson)

    assertThat(result).hasSize(7)
    assertThat(result).isEqualTo(clean)
  }

  @Test
  fun `18_1_0 clean moduleGraphJson parses successfully`() {
    val result = parser.parseModuleGraph(cleanGraphJson)

    assertThat(result).hasSize(7)
  }

  @Test
  fun `semantically identical graph across bazel-diff versions reports no module changes`() {
    val fromGraph = parser.parseModuleGraph(pollutedGraphJson)
    val toGraph = parser.parseModuleGraph(cleanGraphJson)

    val changed = parser.findChangedModules(fromGraph, toGraph)

    assertThat(changed).isEmpty()
  }

  @Test
  fun `string compare at line 44 fires because polluted != clean even when modules are identical`() {
    // Documents that the naive byte compare in CalculateImpactedTargetsInteractor.execute
    // still fires cross-version, so downstream dispatch must keep handling it gracefully.
    assertThat(pollutedGraphJson).isNotEqualTo(cleanGraphJson)
  }

  @Test
  fun `large head graph produces no spurious fan-out`() {
    val deps = (1..100).joinToString(",\n") { i ->
      """          {"key": "mod$i@1.0", "name": "mod$i", "version": "1.0", "apparentName": "mod$i"}"""
    }
    val bigHeadGraph = """
      {
        "key": "<root>",
        "name": "my-workspace",
        "version": "",
        "apparentName": "my-workspace",
        "dependencies": [
$deps
        ]
      }
      """.trimIndent()
    val bigPolluted = "$stderrPrefix\n$bigHeadGraph"

    val fromGraph = parser.parseModuleGraph(bigPolluted)
    val toGraph = parser.parseModuleGraph(bigHeadGraph)
    val changed = parser.findChangedModules(fromGraph, toGraph)

    assertThat(changed).isEmpty()
  }

  @Test
  fun `end-to-end - semantically identical graph compared across versions reports no impacted targets`() {
    val hashes = mapOf(
        "//:target1" to TargetHash("", "unchanged1", "unchanged1"),
        "//:target2" to TargetHash("", "unchanged2", "unchanged2"),
        "//:target3" to TargetHash("", "unchanged3", "unchanged3"),
    )

    val outputWriter = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = pollutedGraphJson,
        toModuleGraphJson = cleanGraphJson,
    )

    val impacted = outputWriter.toString().trim().split("\n").filter { it.isNotEmpty() }
    assertThat(impacted).isEmpty()
  }

  @Test
  fun `end-to-end - same bazel-diff version on both sides is fast path`() {
    val hashes = mapOf(
        "//:target1" to TargetHash("", "unchanged1", "unchanged1"),
        "//:target2" to TargetHash("", "unchanged2", "unchanged2"),
    )

    val outputWriter = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = hashes,
        to = hashes,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = cleanGraphJson,
        toModuleGraphJson = cleanGraphJson,
    )

    val impacted = outputWriter.toString().trim().split("\n").filter { it.isNotEmpty() }
    assertThat(impacted).isEmpty()
  }
}
