package com.bazel_diff.interactor

import assertk.assertFailure
import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.containsExactly
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.hasMessage
import assertk.assertions.hasSize
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isInstanceOf
import assertk.assertions.isTrue
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.testModule
import org.junit.Rule
import org.junit.Test
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule

/**
 * The scenario throughout mirrors issue #479: a common module's generated sources feed a shared
 * library, which feeds service A. Service A can be impacted because it changed itself, because the
 * common module changed, or both.
 */
class ExplainImpactInteractorTest : KoinTest {
  @get:Rule val koinTestRule = KoinTestRule.create { modules(testModule()) }

  private val interactor = ExplainImpactInteractor()

  // //common:util.py -> //common:gen -> //common:lib -> //service/a:app
  private val depEdges =
      mapOf(
          "//service/a:app" to listOf("//common:lib", "//service/a:main.py"),
          "//common:lib" to listOf("//common:gen"),
          "//common:gen" to listOf("//common:util.py"),
      )

  private fun hashes(vararg entries: Pair<String, Pair<String, String>>) =
      entries.associate { (label, pair) ->
        val type = if (label.endsWith(".py")) "SourceFile" else "Rule"
        label to TargetHash(type, pair.first, pair.second)
      }

  private fun unchangedBase() =
      hashes(
          "//service/a:app" to ("app" to "app-direct"),
          "//service/a:main.py" to ("main" to "main"),
          "//common:lib" to ("lib" to "lib-direct"),
          "//common:gen" to ("gen" to "gen-direct"),
          "//common:util.py" to ("util" to "util"),
      )

  @Test
  fun attributesAnIndirectImpactToTheUpstreamSourceChange() {
    val from = unchangedBase()
    // util.py's content changed; the change propagates up through gen and lib into the app. Only
    // util.py's directHash moves -- everything downstream sees a transitive-hash change only.
    val to =
        hashes(
            "//service/a:app" to ("app2" to "app-direct"),
            "//service/a:main.py" to ("main" to "main"),
            "//common:lib" to ("lib2" to "lib-direct"),
            "//common:gen" to ("gen2" to "gen-direct"),
            "//common:util.py" to ("util2" to "util2"),
        )

    val result = interactor.explain(from, to, depEdges, "//service/a:app")

    assertThat(result.impacted).isTrue()
    assertThat(result.directlyChanged).isFalse()
    assertThat(result.totalRootCauses).isEqualTo(1)
    val cause = result.rootCauses.single()
    assertThat(cause.label).isEqualTo("//common:util.py")
    assertThat(cause.kind).isEqualTo(RootCauseKind.SELF_CHANGED)
    assertThat(cause.targetDistance).isEqualTo(3)
    // Every hop crosses //common -> //common -> //service/a, so exactly one boundary is crossed.
    assertThat(cause.packageHops).isEqualTo(1)
    assertThat(cause.path)
        .containsExactly("//common:util.py", "//common:gen", "//common:lib", "//service/a:app")
  }

  @Test
  fun reportsEveryReasonWhenATargetChangedItselfAndUpstream() {
    // The exact ambiguity issue #479 asks about: service A changed AND the common module changed.
    val from = unchangedBase()
    val to =
        hashes(
            "//service/a:app" to ("app2" to "app-direct2"),
            "//service/a:main.py" to ("main2" to "main2"),
            "//common:lib" to ("lib2" to "lib-direct"),
            "//common:gen" to ("gen2" to "gen-direct"),
            "//common:util.py" to ("util2" to "util2"),
        )

    val result = interactor.explain(from, to, depEdges, "//service/a:app")

    assertThat(result.directlyChanged).isTrue()
    assertThat(result.rootCauses.map { it.label })
        .containsExactlyInAnyOrder("//service/a:app", "//service/a:main.py", "//common:util.py")
    // Nearest-first ordering: the target itself, then its own source, then the upstream module.
    assertThat(result.rootCauses.map { it.targetDistance }).containsExactly(0, 1, 3)
  }

  @Test
  fun walksThroughADirectlyChangedDepToFindFurtherRootCauses() {
    // //common:gen changed its own definition AND consumes a changed source. Both are genuine
    // reasons the app moved, so the walk must not stop at the first DIRECT label it reaches.
    val from = unchangedBase()
    val to =
        hashes(
            "//service/a:app" to ("app2" to "app-direct"),
            "//service/a:main.py" to ("main" to "main"),
            "//common:lib" to ("lib2" to "lib-direct"),
            "//common:gen" to ("gen2" to "gen-direct2"),
            "//common:util.py" to ("util2" to "util2"),
        )

    val result = interactor.explain(from, to, depEdges, "//service/a:app")

    assertThat(result.rootCauses.map { it.label })
        .containsExactlyInAnyOrder("//common:gen", "//common:util.py")
  }

  @Test
  fun classifiesATargetAbsentFromTheStartingRevisionAsNew() {
    val from = unchangedBase().filterKeys { it != "//common:util.py" }
    val to =
        hashes(
            "//service/a:app" to ("app2" to "app-direct"),
            "//service/a:main.py" to ("main" to "main"),
            "//common:lib" to ("lib2" to "lib-direct"),
            "//common:gen" to ("gen2" to "gen-direct"),
            "//common:util.py" to ("util" to "util"),
        )

    val result = interactor.explain(from, to, depEdges, "//service/a:app")

    assertThat(result.rootCauses.single().kind).isEqualTo(RootCauseKind.NEW_TARGET)
  }

  @Test
  fun reportsAnUnimpactedTargetAsSuch() {
    val base = unchangedBase()

    val result = interactor.explain(base, base, depEdges, "//service/a:app")

    assertThat(result.impacted).isFalse()
    assertThat(result.rootCauses).isEmpty()
    assertThat(result.nodes).isEmpty()
    assertThat(result.edges).isEmpty()
    assertThat(result.targetType).isEqualTo("Rule")
  }

  @Test
  fun distinguishesADeletedTargetFromAnUnchangedOne() {
    // bazel-diff never reports a deleted target as impacted, but "deleted" and "hash unchanged"
    // are different facts and must not collapse into the same answer.
    val from = unchangedBase()
    val to = from.filterKeys { it != "//service/a:app" }

    val result = interactor.explain(from, to, depEdges, "//service/a:app")

    assertThat(result.impacted).isFalse()
    assertThat(result.removed).isTrue()
  }

  @Test
  fun anUnchangedTargetIsNotReportedAsRemoved() {
    val base = unchangedBase()

    assertThat(interactor.explain(base, base, depEdges, "//service/a:app").removed).isFalse()
  }

  @Test
  fun rejectsALabelPresentInNeitherRevision() {
    val base = unchangedBase()

    assertFailure { interactor.explain(base, base, depEdges, "//typo:nope") }
        .isInstanceOf(UnknownTargetException::class)
        .hasMessage(
            "//typo:nope is not present in either hash file. Check the label spelling, and note " +
                "that hashes generated with --targetType only contain the types you asked for.")
  }

  @Test
  fun truncatesToMaxRootCausesButStillReportsTheTotal() {
    // Five independently-changed sources all feeding one app.
    val sources = (1..5).map { "//common:s$it.py" }
    val edges = mapOf("//app:app" to sources)
    val from =
        (sources.associateWith { TargetHash("SourceFile", "v1", "v1") } +
            mapOf("//app:app" to TargetHash("Rule", "app", "app")))
    val to =
        (sources.associateWith { TargetHash("SourceFile", "v2", "v2") } +
            mapOf("//app:app" to TargetHash("Rule", "app2", "app")))

    val result = interactor.explain(from, to, edges, "//app:app", maxRoots = 2)

    assertThat(result.rootCauses).hasSize(2)
    assertThat(result.totalRootCauses).isEqualTo(5)
    assertThat(result.truncated).isTrue()
    // Nearest-first, then lexicographic -- so the cap is deterministic, not arbitrary.
    assertThat(result.rootCauses.map { it.label })
        .containsExactly("//common:s1.py", "//common:s2.py")
    // The subgraph spans only the reported causes.
    assertThat(result.nodes.map { it.label })
        .containsExactlyInAnyOrder("//app:app", "//common:s1.py", "//common:s2.py")
  }

  @Test
  fun maxDepthBoundsTheSearch() {
    val from = unchangedBase()
    val to =
        hashes(
            "//service/a:app" to ("app2" to "app-direct"),
            "//service/a:main.py" to ("main" to "main"),
            "//common:lib" to ("lib2" to "lib-direct"),
            "//common:gen" to ("gen2" to "gen-direct"),
            "//common:util.py" to ("util2" to "util2"),
        )

    // util.py sits 3 hops up; a 2-hop budget cannot reach it.
    val result = interactor.explain(from, to, depEdges, "//service/a:app", maxDepth = 2)

    assertThat(result.impacted).isTrue()
    assertThat(result.rootCauses).isEmpty()
  }

  @Test
  fun subgraphIncludesEveryEdgeBetweenIncludedNodesNotJustPathEdges() {
    // Diamond: two independent paths from the changed source down to the app. The BFS tree keeps
    // one parent per node, but the rendered subgraph must show both real edges.
    val edges =
        mapOf(
            "//app:app" to listOf("//mid:a", "//mid:b"),
            "//mid:a" to listOf("//src:x.py"),
            "//mid:b" to listOf("//src:x.py"),
        )
    val from =
        mapOf(
            "//app:app" to TargetHash("Rule", "app", "app"),
            "//mid:a" to TargetHash("Rule", "a", "a"),
            "//mid:b" to TargetHash("Rule", "b", "b"),
            "//src:x.py" to TargetHash("SourceFile", "x", "x"),
        )
    val to =
        mapOf(
            "//app:app" to TargetHash("Rule", "app2", "app"),
            "//mid:a" to TargetHash("Rule", "a2", "a"),
            "//mid:b" to TargetHash("Rule", "b2", "b"),
            "//src:x.py" to TargetHash("SourceFile", "x2", "x2"),
        )

    val result = interactor.explain(from, to, edges, "//app:app")

    assertThat(result.rootCauses.single().label).isEqualTo("//src:x.py")
    // Only the BFS-tree parent of x.py is on the reported path, so just that mid node is included.
    val nodeLabels = result.nodes.map { it.label }
    assertThat(nodeLabels).contains("//app:app")
    assertThat(nodeLabels).contains("//src:x.py")
    // Every edge is oriented root -> target (the reverse of the dep-edges file).
    result.edges.forEach { edge -> assertThat(edges.getValue(edge.to)).contains(edge.from) }
  }

  @Test
  fun reportsNoRootCauseWhenNoDepIsImpacted() {
    // The #268 shape: the hash JSON was filtered by --targetType so the changed source is absent,
    // leaving an impacted Rule whose deps are all unimpacted. Must degrade, not crash.
    val from = mapOf("//app:app" to TargetHash("Rule", "app", "app"))
    val to = mapOf("//app:app" to TargetHash("Rule", "app2", "app"))

    val result =
        interactor.explain(from, to, mapOf("//app:app" to listOf("//src:x.py")), "//app:app")

    assertThat(result.impacted).isTrue()
    assertThat(result.directlyChanged).isFalse()
    assertThat(result.rootCauses).isEmpty()
  }

  @Test
  fun handlesAnEmptyTargetTypeWhenHashesWereGeneratedWithoutIt() {
    val from = mapOf("//app:app" to TargetHash("", "app", "app"))
    val to = mapOf("//app:app" to TargetHash("", "app2", "app2"))

    val result = interactor.explain(from, to, emptyMap(), "//app:app")

    assertThat(result.targetType).isEqualTo("")
    assertThat(result.rootCauses.single().targetType).isEqualTo("")
  }
}
