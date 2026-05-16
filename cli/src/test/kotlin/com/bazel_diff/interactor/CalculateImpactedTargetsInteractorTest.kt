package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.*
import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.bazel.BazelTarget
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.testModule
import java.io.StringWriter
import org.junit.Rule
import org.junit.Test
import org.koin.core.context.loadKoinModules
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.mockito.junit.MockitoJUnit
import org.mockito.kotlin.any
import org.mockito.kotlin.doAnswer
import org.mockito.kotlin.mock

class CalculateImpactedTargetsInteractorTest : KoinTest {
  @get:Rule val mockitoRule = MockitoJUnit.rule()

  @get:Rule val koinTestRule = KoinTestRule.create { modules(testModule()) }

  @Test
  fun testGetImpactedTargets() {
    val interactor = CalculateImpactedTargetsInteractor()
    val start: MutableMap<String, String> = HashMap()
    start["1"] = "a"
    start["2"] = "b"
    val startHashes = start.mapValues { TargetHash("", it.value, it.value) }

    val end: MutableMap<String, String> = HashMap()
    end["1"] = "c"
    end["2"] = "b"
    end["3"] = "d"
    val endHashes = end.mapValues { TargetHash("", it.value, it.value) }

    val impacted = interactor.computeSimpleImpactedTargets(startHashes, endHashes)
    assertThat(impacted).containsExactlyInAnyOrder("1", "3")
  }

  @Test
  fun testExecuteSortsByKindThenLabel() {
    val startHashes =
        mapOf(
            "//pkg:rule_a" to TargetHash("Rule", "r_a", "r_a"),
            "//pkg:rule_b" to TargetHash("Rule", "r_b", "r_b"),
            "//pkg:gen_a" to TargetHash("GeneratedFile", "g_a", "g_a"),
            "//pkg:gen_b" to TargetHash("GeneratedFile", "g_b", "g_b"),
            "//pkg:src_a" to TargetHash("SourceFile", "s_a", "s_a"),
            "//pkg:src_b" to TargetHash("SourceFile", "s_b", "s_b"),
        )
    val endHashes = startHashes.mapValues { (_, v) -> v.copy(hash = v.hash + "-changed") }

    val outputWriter = StringWriter()
    CalculateImpactedTargetsInteractor()
        .execute(
            from = startHashes,
            to = endHashes,
            outputWriter = outputWriter,
            targetTypes = null,
        )

    val lines = outputWriter.toString().trimEnd('\n').split("\n")
    assertThat(lines)
        .containsExactly(
            "//pkg:src_a",
            "//pkg:src_b",
            "//pkg:gen_a",
            "//pkg:gen_b",
            "//pkg:rule_a",
            "//pkg:rule_b",
        )
  }

  @Test
  fun testOmitsUnchangedTargets() {
    val (depEdges, startHashes) =
        createTargetHashes(
            "//:1 <- //:2 <- //:3",
            "//:unchanged <- //:3",
        )

    val endHashes = startHashes.toMutableMap()

    makeDirectlyChanged(endHashes, "//:1", "//:2")
    makeIndirectlyChanged(endHashes, "//:3")

    val interactor = CalculateImpactedTargetsInteractor()
    val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

    assertThat(impacted)
        .containsOnly(
            "//:1" to TargetDistanceMetrics(0, 0),
            "//:2" to TargetDistanceMetrics(0, 0),
            "//:3" to TargetDistanceMetrics(1, 0),
        )
  }

  @Test
  fun testNewTargetsDirectlyModified() {
    val startHashes: Map<String, TargetHash> = mapOf()

    val endHashes =
        mapOf(
            "//:1" to TargetHash("", "a", "a"),
            "//:2" to TargetHash("", "b", "b"),
        )

    val interactor = CalculateImpactedTargetsInteractor()
    val impacted = interactor.computeAllDistances(startHashes, endHashes, mapOf())

    assertThat(impacted)
        .containsOnly(
            "//:1" to TargetDistanceMetrics(0, 0),
            "//:2" to TargetDistanceMetrics(0, 0),
        )
  }

  @Test
  fun testTargetDistances() {
    var (depEdges, startHashes) = createTargetHashes("//:1 <- //:2 <- //:3")
    val endHashes = startHashes.toMutableMap()

    makeDirectlyChanged(endHashes, "//:1")
    makeIndirectlyChanged(endHashes, "//:2", "//:3")

    val interactor = CalculateImpactedTargetsInteractor()
    val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

    assertThat(impacted)
        .containsOnly(
            "//:1" to TargetDistanceMetrics(0, 0),
            "//:2" to TargetDistanceMetrics(1, 0),
            "//:3" to TargetDistanceMetrics(2, 0),
        )
  }

  @Test
  fun testPackageDistance() {
    var (depEdges, startHashes) = createTargetHashes("//A:1 <- //A:2 <- //B:3 <- //B:4 <- //C:5")
    val endHashes = startHashes.toMutableMap()

    makeDirectlyChanged(endHashes, "//A:1")
    makeIndirectlyChanged(endHashes, "//A:2", "//B:3", "//B:4", "//C:5")

    val interactor = CalculateImpactedTargetsInteractor()
    val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

    assertThat(impacted)
        .containsOnly(
            "//A:1" to TargetDistanceMetrics(0, 0),
            "//A:2" to TargetDistanceMetrics(1, 0),
            "//B:3" to TargetDistanceMetrics(2, 1),
            "//B:4" to TargetDistanceMetrics(3, 1),
            "//C:5" to TargetDistanceMetrics(4, 2),
        )
  }

  @Test
  fun testFindsShortestDistances() {
    // Test that we find the shortest target and package distance, even if they each pass
    // through different dependency chains.
    //
    // //final:final's targetDistance should be 4 due to passing through //B, //C, and //D,
    // but its packageDistance should be 2 due to passing through //A.
    val (depEdges, startHashes) =
        createTargetHashes(
            "//changed:target <- //A:target_1 <- //A:target_2 <- //A:target_3 <- //A:target_4 <- //final:final",
            "//changed:target <- //B:target <- //C:target <- //D:target <- //final:final",
        )
    val endHashes = startHashes.toMutableMap()

    makeDirectlyChanged(endHashes, "//changed:target")
    makeIndirectlyChanged(
        endHashes,
        "//A:target_1",
        "//A:target_2",
        "//A:target_3",
        "//A:target_4",
        "//B:target",
        "//C:target",
        "//D:target",
        "//final:final")

    val interactor = CalculateImpactedTargetsInteractor()
    val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

    assertThat(impacted["//final:final"]).isEqualTo(TargetDistanceMetrics(4, 2))
  }

  @Test
  fun testDependsOnNewTarget() {
    var (depEdges, startHashes) = createTargetHashes("//:1 <- //:2 <- //:3")
    val endHashes = startHashes.toMutableMap()

    // Remove //:1 so that it is a new target in the end hashes, but //:2 is
    // only indirectly modified.
    // Technically, this probably can't happen, as //:2 would have to have had
    // one of its deps attrs modified, which would have caused it to be directly
    // modified, but we should handle it anyway.
    startHashes.remove("//:1")

    makeIndirectlyChanged(endHashes, "//:2", "//:3")

    val interactor = CalculateImpactedTargetsInteractor()
    val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

    assertThat(impacted)
        .containsOnly(
            "//:1" to TargetDistanceMetrics(0, 0),
            "//:2" to TargetDistanceMetrics(1, 0),
            "//:3" to TargetDistanceMetrics(2, 0),
        )
  }

  @Test
  fun testInvalidEdgesFallsBackToDistanceZero() {
    // Regression coverage for the issue-#268 fix: when an indirectly impacted target has
    // either no dep-edges entry OR no impacted predecessors in its dep-edges (typically
    // because the user filtered the hash JSON with --targetType=Rule and the only changed
    // dep is a non-Rule like a GeneratedFile), `computeAllDistances` should log a warning
    // and report the target at distance 0 instead of throwing.
    var (depEdges, startHashes) = createTargetHashes("//:1 <- //:2")
    val endHashes = startHashes.toMutableMap()

    makeIndirectlyChanged(endHashes, "//:2")

    val interactor = CalculateImpactedTargetsInteractor()

    // Case A: //:2 has a dep-edges entry (//:1) but //:1 is not impacted in this scenario,
    // so there are no impacted deps. Should fall back to distance 0, not throw.
    val withEdges = interactor.computeAllDistances(startHashes, endHashes, depEdges)
    assertThat(withEdges).containsOnly("//:2" to TargetDistanceMetrics(0, 0))

    // Case B: //:2 has no dep-edges entry at all (empty map). Same fallback.
    val withoutEdges = interactor.computeAllDistances(startHashes, endHashes, mapOf())
    assertThat(withoutEdges).containsOnly("//:2" to TargetDistanceMetrics(0, 0))
  }

  /**
   * Creates a mapping of target hashes and dependency edges from the provided graph specifications.
   *
   * @param graphSpecs Vararg parameter representing the graph specifications. Each specification is
   *   a string where targets are separated by " <- " indicating a dependency relationship.
   * @return A pair consisting of:
   *     - A map where the keys are target labels and the values are lists of labels that depend on
   *       the key.
   *     - A map where the keys are target labels and the values are `TargetHash` objects
   *       representing the target hashes.
   */
  fun createTargetHashes(
      vararg graphSpecs: String
  ): Pair<Map<String, List<String>>, MutableMap<String, TargetHash>> {
    val targetHashMap = mutableMapOf<String, TargetHash>()
    val depEdges = mutableMapOf<String, MutableList<String>>()

    for (spec in graphSpecs) {
      val labels = spec.split(" <- ")
      labels.zipWithNext().forEach { (prevLabel, label) ->
        if (!targetHashMap.containsKey(prevLabel)) {
          targetHashMap[prevLabel] = TargetHash("", prevLabel, prevLabel)
        }

        if (!targetHashMap.containsKey(label)) {
          targetHashMap[label] = TargetHash("", label, label)
        }

        depEdges.computeIfAbsent(label) { mutableListOf() }.add(prevLabel)
      }
    }

    return depEdges to targetHashMap
  }

  fun makeDirectlyChanged(targetHashes: MutableMap<String, TargetHash>, vararg labels: String) {
    for (label in labels) {
      if (!targetHashes.containsKey(label)) {
        throw IllegalArgumentException("Label $label not found in target hashes")
      }
      val orig = targetHashes[label]!!
      targetHashes[label] =
          orig.copy(directHash = orig.directHash + "-changed", hash = orig.hash + "-changed")
    }
  }

  fun makeIndirectlyChanged(targetHashes: MutableMap<String, TargetHash>, vararg labels: String) {
    for (label in labels) {
      if (!targetHashes.containsKey(label)) {
        throw IllegalArgumentException("Label $label not found in target hashes")
      }
      val orig = targetHashes[label]!!
      targetHashes[label] = orig.copy(hash = orig.hash + "-changed")
    }
  }

  @Test
  fun testModuleChangesWithoutWorkspace() {
    // When module changes occur and query service is not available, all targets are marked as impacted
    val startHashes = mapOf(
        "//:target1" to TargetHash("", "hash1", "hash1"),
        "//:target2" to TargetHash("", "hash2", "hash2"),
        "@@abseil-cpp~20240116.2//:strings" to TargetHash("", "ext1", "ext1")
    )

    val endHashes = mapOf(
        "//:target1" to TargetHash("", "hash1", "hash1"),
        "//:target2" to TargetHash("", "hash2", "hash2"),
        "@@abseil-cpp~20240722.0//:strings" to TargetHash("", "ext2", "ext2")
    )

    val fromModuleGraph = """
      {
        "key": "root",
        "name": "root",
        "version": "",
        "apparentName": "root",
        "dependencies": [
          {"key": "bazel_tools@_", "name": "bazel_tools", "version": "_", "apparentName": "bazel_tools"},
          {"key": "abseil-cpp@20240116.2", "name": "abseil-cpp", "version": "20240116.2", "apparentName": "abseil-cpp"}
        ]
      }
    """.trimIndent()

    val toModuleGraph = """
      {
        "key": "root",
        "name": "root",
        "version": "",
        "apparentName": "root",
        "dependencies": [
          {"key": "bazel_tools@_", "name": "bazel_tools", "version": "_", "apparentName": "bazel_tools"},
          {"key": "abseil-cpp@20240722.0", "name": "abseil-cpp", "version": "20240722.0", "apparentName": "abseil-cpp"}
        ]
      }
    """.trimIndent()

    val outputWriter = StringWriter()
    val interactor = CalculateImpactedTargetsInteractor()

    interactor.execute(
        from = startHashes,
        to = endHashes,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = fromModuleGraph,
        toModuleGraphJson = toModuleGraph
    )

    val output = outputWriter.toString().trim().split("\n")
    // Module changes detected but no query service available - all targets including external are impacted
    assertThat(output).containsExactlyInAnyOrder("//:target1", "//:target2", "@@abseil-cpp~20240722.0//:strings")
  }

  @Test
  fun testModuleChangesWithWorkspaceButNoQueryService() {
    // When module changes are detected and workspace is provided but query service not available,
    // should fall back to marking all targets as impacted (including external targets)
    val startHashes = mapOf(
        "//:target1" to TargetHash("", "hash1", "hash1"),
        "//:target2" to TargetHash("", "hash2", "hash2"),
        "@@abseil-cpp~20240116.2//:strings" to TargetHash("", "ext1", "ext1")
    )

    val endHashes = mapOf(
        "//:target1" to TargetHash("", "hash1", "hash1"),
        "//:target2" to TargetHash("", "hash2", "hash2"),
        "@@abseil-cpp~20240722.0//:strings" to TargetHash("", "ext2", "ext2")
    )

    val fromModuleGraph = """
      {
        "key": "root",
        "name": "root",
        "version": "",
        "apparentName": "root",
        "dependencies": [
          {"key": "abseil-cpp@20240116.2", "name": "abseil-cpp", "version": "20240116.2", "apparentName": "abseil-cpp"}
        ]
      }
    """.trimIndent()

    val toModuleGraph = """
      {
        "key": "root",
        "name": "root",
        "version": "",
        "apparentName": "root",
        "dependencies": [
          {"key": "abseil-cpp@20240722.0", "name": "abseil-cpp", "version": "20240722.0", "apparentName": "abseil-cpp"}
        ]
      }
    """.trimIndent()

    // No BazelQueryService in the test module, so should fall back to all targets
    val outputWriter = StringWriter()
    val interactor = CalculateImpactedTargetsInteractor()

    interactor.execute(
        from = startHashes,
        to = endHashes,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = fromModuleGraph,
        toModuleGraphJson = toModuleGraph
    )

    val output = outputWriter.toString().trim().split("\n")
    // All targets including external should be marked as impacted when query service not available
    assertThat(output).containsExactlyInAnyOrder("//:target1", "//:target2", "@@abseil-cpp~20240722.0//:strings")
  }

  @Test
  fun testNoModuleChanges() {
    // When no module changes occur, should use normal hash comparison
    val startHashes = mapOf(
        "//:target1" to TargetHash("", "hash1", "hash1"),
        "//:target2" to TargetHash("", "hash2", "hash2")
    )

    val endHashes = mapOf(
        "//:target1" to TargetHash("", "hash1-changed", "hash1-changed"),
        "//:target2" to TargetHash("", "hash2", "hash2")
    )

    val moduleGraph = """
      {
        "key": "root",
        "dependencies": [
          {"key": "abseil-cpp@20240116.2", "name": "abseil-cpp", "version": "20240116.2"}
        ]
      }
    """.trimIndent()

    val outputWriter = StringWriter()
    val interactor = CalculateImpactedTargetsInteractor()

    interactor.execute(
        from = startHashes,
        to = endHashes,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = moduleGraph,
        toModuleGraphJson = moduleGraph  // Same module graph
    )

    val output = outputWriter.toString().trim().split("\n")
    // Only target1 should be impacted (hash changed)
    assertThat(output).containsExactly("//:target1")
  }

  @Test
  fun testModuleChangesWithDistances() {
    // Test executeWithDistances with module changes - when query service is not available, all targets are marked as impacted
    val startHashes = mapOf(
        "//:1" to TargetHash("", "//:1", "//:1"),
        "//:2" to TargetHash("", "//:2", "//:2"),
        "//:3" to TargetHash("", "//:3", "//:3")
    )

    val endHashes = mapOf(
        "//:1" to TargetHash("", "//:1-changed", "//:1-changed"),
        "//:2" to TargetHash("", "//:2", "//:2"),
        "//:3" to TargetHash("", "//:3", "//:3")
    )

    val fromModuleGraph = """
      {
        "key": "root",
        "name": "root",
        "version": "",
        "apparentName": "root",
        "dependencies": [
          {"key": "test-module@1.0", "name": "test-module", "version": "1.0", "apparentName": "test-module"}
        ]
      }
    """.trimIndent()

    val toModuleGraph = """
      {
        "key": "root",
        "name": "root",
        "version": "",
        "apparentName": "root",
        "dependencies": [
          {"key": "test-module@2.0", "name": "test-module", "version": "2.0", "apparentName": "test-module"}
        ]
      }
    """.trimIndent()

    val outputWriter = StringWriter()
    val interactor = CalculateImpactedTargetsInteractor()

    interactor.executeWithDistances(
        from = startHashes,
        to = endHashes,
        depEdges = mapOf(), // No dep edges needed
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = fromModuleGraph,
        toModuleGraphJson = toModuleGraph
    )

    val output = outputWriter.toString()
    // Module changes detected but no query service available - all targets are marked as impacted
    assertThat(output).contains("//:1")
    assertThat(output).contains("//:2")
    assertThat(output).contains("//:3")
    assertThat(output).contains("\"targetDistance\": 0")
    assertThat(output).contains("\"packageDistance\": 0")
  }

  @Test
  fun testMissingModuleGraph() {
    // When module graph is missing, should fall back to normal hash comparison
    val startHashes = mapOf(
        "//:target1" to TargetHash("", "hash1", "hash1"),
        "//:target2" to TargetHash("", "hash2", "hash2")
    )

    val endHashes = mapOf(
        "//:target1" to TargetHash("", "hash1-changed", "hash1-changed"),
        "//:target2" to TargetHash("", "hash2", "hash2")
    )

    val outputWriter = StringWriter()
    val interactor = CalculateImpactedTargetsInteractor()

    interactor.execute(
        from = startHashes,
        to = endHashes,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = null,  // Missing module graph
        toModuleGraphJson = null
    )

    val output = outputWriter.toString().trim().split("\n")
    // Only target1 should be impacted (hash changed)
    assertThat(output).containsExactly("//:target1")
  }

  @Test
  fun testExecuteFiltersExternalLabelsWhenExcludeFlagSet() {
    // Unit-level guard for https://github.com/Tinder/bazel-diff/issues/326. The synthetic
    // //external:<apparent_name> labels produced for bzlmod repos must drop out of the
    // impacted-targets output when --excludeExternalTargets is set, while real workspace
    // labels (//foo:bar) and canonical bzlmod labels (@@repo//pkg:tgt) remain.
    val from =
        mapOf(
            "//foo:bar" to TargetHash("Rule", "h1", "h1"),
            "//external:boost.assert" to TargetHash("Rule", "h2", "h2"),
            "//external:guava" to TargetHash("Rule", "h3", "h3"),
            "@@some_repo//pkg:tgt" to TargetHash("Rule", "h4", "h4"),
        )
    val to = from.mapValues { (_, v) -> v.copy(hash = v.hash + "-changed") }

    val filteredWriter = StringWriter()
    CalculateImpactedTargetsInteractor()
        .execute(
            from = from,
            to = to,
            outputWriter = filteredWriter,
            targetTypes = null,
            excludeExternalTargets = true)
    val filteredLines = filteredWriter.toString().trimEnd('\n').split("\n").toSet()
    assertThat(filteredLines).containsOnly("//foo:bar", "@@some_repo//pkg:tgt")

    val unfilteredWriter = StringWriter()
    CalculateImpactedTargetsInteractor()
        .execute(
            from = from,
            to = to,
            outputWriter = unfilteredWriter,
            targetTypes = null,
            excludeExternalTargets = false)
    val unfilteredLines = unfilteredWriter.toString().trimEnd('\n').split("\n").toSet()
    assertThat(unfilteredLines)
        .containsOnly(
            "//foo:bar", "//external:boost.assert", "//external:guava", "@@some_repo//pkg:tgt")
  }

  @Test
  fun testExecuteWithDistancesFiltersExternalLabelsWhenExcludeFlagSet() {
    // Same guarantee for the distance-metrics output path used when --depEdgesFile is provided.
    val from =
        mapOf(
            "//foo:bar" to TargetHash("Rule", "h1", "h1"),
            "//external:boost.assert" to TargetHash("Rule", "h2", "h2"),
        )
    val to = from.mapValues { (_, v) -> v.copy(hash = v.hash + "-changed", directHash = v.directHash + "-changed") }

    val filteredWriter = StringWriter()
    CalculateImpactedTargetsInteractor()
        .executeWithDistances(
            from = from,
            to = to,
            depEdges = mapOf(),
            outputWriter = filteredWriter,
            targetTypes = null,
            excludeExternalTargets = true)
    val filteredJson = filteredWriter.toString()
    assertThat(filteredJson).contains("//foo:bar")
    assertThat(filteredJson).doesNotContain("//external:boost.assert")
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/268
  // ------------------------------------------------------------------------
  // Users who pass `--targetType=Rule` to `generate-hashes` end up with hash JSONs that
  // only contain Rule entries -- SourceFile and GeneratedFile rows are stripped at write
  // time. When that JSON is then fed back into `get-impacted-targets --depsFile=...`
  // (which routes through `executeWithDistances` -> `computeAllDistances`), an indirectly
  // impacted Rule whose only changed dependency is a filtered-out GeneratedFile previously
  // triggered:
  //
  //   InvalidDependencyEdgesException("<label> was indirectly impacted, but has no impacted dependencies.")
  //
  // and crashed the whole job. The fix (this PR) replaces that throw with a logger.w() that
  // points at --targetType as the most likely cause, and returns a conservative
  // TargetDistanceMetrics(0, 0) so the indirectly impacted Rule still appears in the
  // impacted-targets output. @agustinmista's exact scenario in the issue thread.
  @Test
  fun computeAllDistances_targetTypeFilteredDep_fallsBackToDistanceZero_regressionForIssue268() {
    // Setup mirrors the user's scenario:
    //   //pkg:rule (Rule) depends on //pkg:generated (GeneratedFile). The user ran
    //   `generate-hashes --targetType=Rule`, so //pkg:generated never made it into the
    //   `from`/`to` hash JSONs. The underlying source feeding //pkg:generated changed,
    //   so //pkg:rule's *transitive* hash flipped (indirect impact) while its directHash
    //   stayed put. The dep edges file, on the other hand, still records the
    //   Rule -> GeneratedFile edge because it was generated without the type filter.
    val startHashes =
        mutableMapOf(
            "//pkg:rule" to TargetHash("Rule", "rule-direct-hash", "rule-transitive-hash-v1"),
        )
    val endHashes =
        mutableMapOf(
            "//pkg:rule" to TargetHash("Rule", "rule-direct-hash", "rule-transitive-hash-v2"),
        )
    val depEdges =
        mapOf<String, List<String>>(
            "//pkg:rule" to listOf("//pkg:generated"),
        )

    val interactor = CalculateImpactedTargetsInteractor()

    // Pre-fix this would throw InvalidDependencyEdgesException. Post-fix the call returns
    // a conservative distance-0 entry for //pkg:rule (warning logged) so the indirectly
    // impacted Rule still surfaces in the output.
    val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)
    assertThat(impacted).containsOnly("//pkg:rule" to TargetDistanceMetrics(0, 0))
  }

  @Test
  fun testIdenticalModuleGraphsSkipsParsing() {
    // When module graphs are identical, should skip parsing and use normal hash comparison
    // This is an optimization to avoid expensive JSON parsing when modules haven't changed
    val startHashes = mapOf(
        "//:target1" to TargetHash("", "hash1", "hash1"),
        "//:target2" to TargetHash("", "hash2", "hash2")
    )

    val endHashes = mapOf(
        "//:target1" to TargetHash("", "hash1-changed", "hash1-changed"),
        "//:target2" to TargetHash("", "hash2", "hash2")
    )

    val moduleGraph = """
      {
        "key": "root",
        "name": "root",
        "version": "",
        "apparentName": "root",
        "dependencies": [
          {"key": "abseil-cpp@20240116.2", "name": "abseil-cpp", "version": "20240116.2", "apparentName": "abseil-cpp"}
        ]
      }
    """.trimIndent()

    val outputWriter = StringWriter()
    val interactor = CalculateImpactedTargetsInteractor()

    interactor.execute(
        from = startHashes,
        to = endHashes,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = moduleGraph,  // Identical module graphs
        toModuleGraphJson = moduleGraph
    )

    val output = outputWriter.toString().trim().split("\n")
    // Only target1 should be impacted (hash changed) - module logic was skipped
    assertThat(output).containsExactly("//:target1")
  }

  @Test
  fun testUnionsRdepsAcrossChangedModules() {
    // Guard against regressing to per-repo fan-out. Two changed modules matching two
    // canonical repos should produce a single unioned rdeps query, not two.
    val captured = mutableListOf<String>()
    val fakeQueryService: BazelQueryService = mock {
      onBlocking { query(any(), any()) } doAnswer {
        captured.add(it.getArgument(0))
        emptyList<BazelTarget>()
      }
    }
    loadKoinModules(module { single { fakeQueryService } })

    val from = mapOf(
        "//:target1" to TargetHash("", "a", "a"),
        "@@abseil-cpp~20240116.2//:strings" to TargetHash("", "e1", "e1"),
        "@@aspect_bazel_lib~2.22.5//:copy_to_bin" to TargetHash("", "e2", "e2"),
    )
    val to = mapOf(
        "//:target1" to TargetHash("", "a", "a"),
        "@@abseil-cpp~20240722.0//:strings" to TargetHash("", "e1b", "e1b"),
        "@@aspect_bazel_lib~2.23.0//:copy_to_bin" to TargetHash("", "e2b", "e2b"),
    )
    val fromGraph = """
      {
        "key": "root", "name": "root", "version": "", "apparentName": "root",
        "dependencies": [
          {"key": "abseil-cpp@20240116.2", "name": "abseil-cpp", "version": "20240116.2", "apparentName": "abseil-cpp"},
          {"key": "aspect_bazel_lib@2.22.5", "name": "aspect_bazel_lib", "version": "2.22.5", "apparentName": "aspect_bazel_lib"}
        ]
      }
    """.trimIndent()
    val toGraph = """
      {
        "key": "root", "name": "root", "version": "", "apparentName": "root",
        "dependencies": [
          {"key": "abseil-cpp@20240722.0", "name": "abseil-cpp", "version": "20240722.0", "apparentName": "abseil-cpp"},
          {"key": "aspect_bazel_lib@2.23.0", "name": "aspect_bazel_lib", "version": "2.23.0", "apparentName": "aspect_bazel_lib"}
        ]
      }
    """.trimIndent()

    val outputWriter = StringWriter()
    CalculateImpactedTargetsInteractor().execute(
        from = from,
        to = to,
        outputWriter = outputWriter,
        targetTypes = null,
        fromModuleGraphJson = fromGraph,
        toModuleGraphJson = toGraph,
    )

    // Exactly one query - the whole point of this test. The per-repo loop would emit two.
    assertThat(captured).hasSize(1)
    val queryExpression = captured.single()
    assertThat(queryExpression).startsWith("rdeps(//..., ")
    assertThat(queryExpression).endsWith(")")
    // Both matched canonical repos must appear, joined by " + ".
    assertThat(queryExpression).contains("@@abseil-cpp~20240722.0//...")
    assertThat(queryExpression).contains("@@aspect_bazel_lib~2.23.0//...")
    assertThat(queryExpression).contains(" + ")
  }

  // ------------------------------------------------------------------------
  // Reproducer for https://github.com/Tinder/bazel-diff/issues/335 fix #2
  // ------------------------------------------------------------------------
  // When the two module-graph JSON payloads passed to get-impacted-targets are not
  // byte-equal but one of them fails to parse, `findChangedModules(emptyMap, fullMap)`
  // reports every module in the successfully-parsed graph as "added" and the impacted
  // set explodes:
  //
  //   * With a BazelQueryService bound, every "added" module fans out into an rdeps
  //     query against its canonical repo(s). On the workspace in #335 that produced
  //     ~5,000 serial subprocesses and the run took multiple hours.
  //   * With no BazelQueryService bound (or one that errors), the failure-tolerant
  //     fallback path returns `allTargets.keys` -- every hashed label is reported as
  //     impacted, which on a large workspace defeats the point of running bazel-diff.
  //
  // Both outcomes are far worse than the per-target hash diff that would have run if
  // bazel-diff hadn't been told there was a module graph at all.
  //
  // Today this asymmetry was caused by the stderr-pollution shape #336 fixed (an
  // 18.0.x base graph fed into 18.1.0+ would parse to empty before that PR). It can
  // still happen for genuinely unparseable input: a truncated/corrupted base graph
  // pulled out of object storage, a future bazel-mod-graph serialisation change, or
  // a base graph captured before bazel itself learned to emit `--output=json`.
  //
  // Fix #2 from the issue: when one parsed map is empty and the other is not, while
  // `fromModuleGraphJson != toModuleGraphJson`, fall back to `computeSimpleImpactedTargets`
  // instead of treating every module in the populated graph as "added". A per-target
  // hash diff is bounded by the size of the hash set; the module-fan-out path is not.
  //
  // The reproducer is `@Ignore`d so CI stays green. Drop the annotation once the
  // asymmetry-detection fallback lands and confirm this test passes: only the target
  // whose hash actually changed should appear in the impacted set.
  @Test
  @org.junit.Ignore(
      "Reproducer for https://github.com/Tinder/bazel-diff/issues/335 fix #2 " +
          "(parse-asymmetry should fall back to computeSimpleImpactedTargets). " +
          "Today an unparseable base graph + parseable head graph causes every module " +
          "in the head graph to be treated as 'added', and -- with no BazelQueryService " +
          "bound to handle the rdeps fan-out -- every target ends up reported as " +
          "impacted. The desired behaviour is to detect the asymmetry and fall back " +
          "to a per-target hash diff. Drop @Ignore once the fallback lands.")
  fun execute_parseAsymmetryFallsBackToSimpleHashDiff_reproducerForIssue335Fix2() {
    // Three targets, only //:changed has actually changed.
    val startHashes =
        mapOf(
            "//:changed" to TargetHash("Rule", "h1-old", "h1-old"),
            "//:unchanged_a" to TargetHash("Rule", "h2", "h2"),
            "//:unchanged_b" to TargetHash("Rule", "h3", "h3"),
        )
    val endHashes =
        mapOf(
            "//:changed" to TargetHash("Rule", "h1-new", "h1-new"),
            "//:unchanged_a" to TargetHash("Rule", "h2", "h2"),
            "//:unchanged_b" to TargetHash("Rule", "h3", "h3"),
        )

    // fromGraph has no '{' at all, so ModuleGraphParser.parseModuleGraph falls through
    // to `if (start < 0) return emptyMap()`. Mirrors a corrupted base-graph stored from
    // an old bazel-diff run or a totally different serialisation format.
    val fromModuleGraph = "garbage-non-json-payload"
    // toGraph parses cleanly to two modules; without the fix those two modules look
    // like "added" relative to the empty fromGraph.
    val toModuleGraph =
        """
        {
          "key": "root", "name": "root", "version": "", "apparentName": "root",
          "dependencies": [
            {"key": "modA@1.0", "name": "modA", "version": "1.0", "apparentName": "modA"},
            {"key": "modB@1.0", "name": "modB", "version": "1.0", "apparentName": "modB"}
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

    val impacted = outputWriter.toString().trimEnd('\n').split("\n").filter { it.isNotEmpty() }.toSet()
    // With the fallback: only the actually-changed target is in the impacted set.
    // Without the fallback (today): every hashed target is reported because
    // queryTargetsDependingOnModules returns `allTargets.keys` when no BazelQueryService
    // is bound -- i.e. {//:changed, //:unchanged_a, //:unchanged_b}.
    assertThat(impacted).containsOnly("//:changed")
  }

  // Same asymmetry case as above, but routed through executeWithDistances (the path
  // taken when --depsFile is passed to get-impacted-targets). The fix must cover both
  // execute() and executeWithDistances() -- both branch on `changedModules.isNotEmpty()`
  // and call queryTargetsDependingOnModules independently.
  @Test
  @org.junit.Ignore(
      "Reproducer for https://github.com/Tinder/bazel-diff/issues/335 fix #2 " +
          "via the executeWithDistances() path. Drop @Ignore once the asymmetry " +
          "fallback lands and confirm only the hash-diff target is reported.")
  fun executeWithDistances_parseAsymmetryFallsBackToSimpleHashDiff_reproducerForIssue335Fix2() {
    val startHashes =
        mapOf(
            "//:changed" to TargetHash("Rule", "h1-old", "h1-old"),
            "//:unchanged_a" to TargetHash("Rule", "h2", "h2"),
            "//:unchanged_b" to TargetHash("Rule", "h3", "h3"),
        )
    val endHashes =
        mapOf(
            "//:changed" to TargetHash("Rule", "h1-new", "h1-new"),
            "//:unchanged_a" to TargetHash("Rule", "h2", "h2"),
            "//:unchanged_b" to TargetHash("Rule", "h3", "h3"),
        )

    val fromModuleGraph = "garbage-non-json-payload"
    val toModuleGraph =
        """
        {
          "key": "root", "name": "root", "version": "", "apparentName": "root",
          "dependencies": [
            {"key": "modA@1.0", "name": "modA", "version": "1.0", "apparentName": "modA"}
          ]
        }
        """
            .trimIndent()

    val outputWriter = StringWriter()
    CalculateImpactedTargetsInteractor()
        .executeWithDistances(
            from = startHashes,
            to = endHashes,
            depEdges = emptyMap(),
            outputWriter = outputWriter,
            targetTypes = null,
            fromModuleGraphJson = fromModuleGraph,
            toModuleGraphJson = toModuleGraph,
        )

    val output = outputWriter.toString()
    // With the fallback: only //:changed appears in the distance-metrics JSON.
    // Without the fallback: every target is reported, all at distance 0 (because the
    // "module-impacted" branch in executeWithDistances forces distance 0).
    assertThat(output).contains("//:changed")
    assertThat(output).doesNotContain("//:unchanged_a")
    assertThat(output).doesNotContain("//:unchanged_b")
  }
}
