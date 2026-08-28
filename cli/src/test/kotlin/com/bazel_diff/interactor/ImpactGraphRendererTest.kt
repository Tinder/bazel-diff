package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.doesNotContain
import assertk.assertions.isEqualTo
import com.google.gson.GsonBuilder
import org.junit.Test

class ImpactGraphRendererTest {
  private val renderer = ImpactGraphRenderer(GsonBuilder().disableHtmlEscaping().create())

  private fun explanation(
      impacted: Boolean = true,
      removed: Boolean = false,
      directlyChanged: Boolean = false,
      rootCauses: List<RootCause> =
          listOf(
              RootCause(
                  label = "//common:util.py",
                  targetType = "SourceFile",
                  kind = RootCauseKind.SELF_CHANGED,
                  targetDistance = 2,
                  packageHops = 1,
                  path = listOf("//common:util.py", "//common:lib", "//service/a:app"))),
      totalRootCauses: Int = 1,
      truncated: Boolean = false,
  ) =
      ImpactExplanation(
          target = "//service/a:app",
          targetType = "Rule",
          impacted = impacted,
          removed = removed,
          directlyChanged = directlyChanged,
          rootCauses = rootCauses,
          totalRootCauses = totalRootCauses,
          truncated = truncated,
          nodes =
              listOf(
                  ExplainNode("//common:util.py", "SourceFile", "direct", 2, true, false),
                  ExplainNode("//common:lib", "Rule", "indirect", 1, false, false),
                  ExplainNode("//service/a:app", "Rule", "indirect", 0, false, true)),
          edges =
              listOf(
                  ExplainEdge("//common:lib", "//service/a:app"),
                  ExplainEdge("//common:util.py", "//common:lib")))

  @Test
  fun textNamesTheRootCauseTheReasonAndThePath() {
    val out = renderer.render(explanation(), ExplainFormat.TEXT)

    assertThat(out).contains("IMPACTED (indirectly")
    assertThat(out).contains("Root causes: 1")
    assertThat(out).contains("//common:util.py  [SourceFile]")
    assertThat(out).contains("source file content changed")
    assertThat(out).contains("2 hops (1 package boundary crossed)")
    assertThat(out).contains("//common:util.py -> //common:lib -> //service/a:app")
  }

  @Test
  fun textSaysSoWhenTheTargetIsNotImpacted() {
    val out =
        renderer.render(
            explanation(impacted = false, rootCauses = emptyList(), totalRootCauses = 0),
            ExplainFormat.TEXT)

    assertThat(out).contains("NOT IMPACTED -- its hash is identical")
    assertThat(out).doesNotContain("Root causes")
  }

  @Test
  fun textSaysADeletedTargetWasRemovedRatherThanUnchanged() {
    val out =
        renderer.render(
            explanation(
                impacted = false, removed = true, rootCauses = emptyList(), totalRootCauses = 0),
            ExplainFormat.TEXT)

    assertThat(out).contains("exists in the starting revision but not the final one")
    assertThat(out).doesNotContain("hash is identical")
  }

  @Test
  fun textSurfacesTheTruncatedCountRatherThanCappingSilently() {
    val out =
        renderer.render(explanation(totalRootCauses = 9, truncated = true), ExplainFormat.TEXT)

    assertThat(out).contains("Root causes: 9 (showing the 1 nearest)")
    assertThat(out).contains("... and 8 more")
    assertThat(out).contains("--maxRootCauses=0")
  }

  @Test
  fun textPointsAtIssue268WhenNothingCouldBeAttributed() {
    val out =
        renderer.render(
            explanation(rootCauses = emptyList(), totalRootCauses = 0), ExplainFormat.TEXT)

    assertThat(out).contains("No root cause could be attributed")
    assertThat(out).contains("--targetType")
    assertThat(out).contains("issues/268")
  }

  @Test
  fun textDistinguishesADirectSelfChange() {
    val out = renderer.render(explanation(directlyChanged = true), ExplainFormat.TEXT)

    assertThat(out).contains("IMPACTED (directly -- this target changed on its own)")
  }

  @Test
  fun dotEmitsNodesEdgesAndALegend() {
    val out = renderer.render(explanation(), ExplainFormat.DOT)

    assertThat(out).contains("digraph bazel_diff_impact {")
    assertThat(out).contains("rankdir=TB;")
    // Edges run root cause -> queried target: n0 is the most-distant node (util.py), n2 the target.
    assertThat(out).contains("n0 [label=\"//common:util.py\\n(root cause)\"")
    assertThat(out).contains("n2 [label=\"//service/a:app\\n(queried target)\"")
    assertThat(out).contains("n0 -> n1;")
    assertThat(out).contains("n1 -> n2;")
    assertThat(out).contains("subgraph cluster_legend {")
    assertThat(out).contains("root cause")
    assertThat(out).contains("queried target")
  }

  @Test
  fun dotColorsIdentityOnTheStrokeOverAUniformSurfaceFill() {
    val out = renderer.render(explanation(), ExplainFormat.DOT)

    // Identity lives on `color` (the stroke); every node shares one surface fill, so the graph
    // reads the same on any page background and no tint has to carry meaning.
    assertThat(out).contains("color=\"#eb6834\", penwidth=2, peripheries=2")
    assertThat(out).contains("color=\"#2a78d6\", penwidth=2")
    assertThat(out).contains("fillcolor=\"#fcfcfb\"")
    assertThat(out).contains("bgcolor=\"#fcfcfb\";")
  }

  @Test
  fun dotEscapesQuotesAndBackslashesInLabels() {
    val awkward = "//weird:a\"b\\c"
    val out =
        renderer.render(
            explanation()
                .copy(
                    nodes = listOf(ExplainNode(awkward, "Rule", "direct", 0, false, false)),
                    edges = emptyList()),
            ExplainFormat.DOT)

    assertThat(out).contains("n0 [label=\"//weird:a\\\"b\\\\c\"];")
  }

  @Test
  fun mermaidShapesEachRoleDistinctlySoColorIsNeverTheOnlyEncoding() {
    val out = renderer.render(explanation(), ExplainFormat.MERMAID)

    assertThat(out).contains("flowchart TD")
    assertThat(out).contains("n0[[\"//common:util.py<br/>(root cause)\"]]")
    assertThat(out).contains("n1[\"//common:lib\"]")
    assertThat(out).contains("n2([\"//service/a:app<br/>(queried target)\"])")
    assertThat(out).contains("n0 --> n1")
    assertThat(out).contains("class n2 queried")
    assertThat(out).contains("class n0 rootCause")
    assertThat(out).contains("class n1 intermediate")
  }

  @Test
  fun mermaidEscapesQuotesUsingTheEntityFormMermaidUnderstands() {
    val out =
        renderer.render(
            explanation()
                .copy(
                    nodes = listOf(ExplainNode("//weird:a\"b", "Rule", "direct", 0, false, false)),
                    edges = emptyList()),
            ExplainFormat.MERMAID)

    assertThat(out).contains("n0[\"//weird:a#quot;b\"]")
  }

  @Test
  fun mermaidOmitsAClassLineForARoleWithNoNodes() {
    val out =
        renderer.render(
            explanation()
                .copy(
                    nodes = listOf(ExplainNode("//app:app", "Rule", "direct", 0, true, true)),
                    edges = emptyList()),
            ExplainFormat.MERMAID)

    assertThat(out).contains("class n0 queried")
    assertThat(out).doesNotContain("class  intermediate")
    assertThat(out).doesNotContain("class n0 rootCause")
    // A node that is both the target and a root cause says so directly.
    assertThat(out).contains("(queried target - also a root cause)")
  }

  @Test
  fun jsonRoundTripsTheWholeExplanation() {
    val gson = GsonBuilder().disableHtmlEscaping().create()
    val out = ImpactGraphRenderer(gson).render(explanation(), ExplainFormat.JSON)

    val parsed = gson.fromJson(out, ImpactExplanation::class.java)
    assertThat(parsed).isEqualTo(explanation())
  }

  @Test
  fun textPluralisesASingleHopCorrectly() {
    val out =
        renderer.render(
            explanation(
                rootCauses =
                    listOf(
                        RootCause(
                            "//common:lib",
                            "Rule",
                            RootCauseKind.NEW_TARGET,
                            1,
                            0,
                            listOf("//common:lib", "//service/a:app")))),
            ExplainFormat.TEXT)

    assertThat(out).contains("1 hop (0 package boundaries crossed)")
    assertThat(out).contains("new target in the final revision")
  }

  @Test
  fun textDescribesTheQueriedTargetItselfAsAZeroHopCause() {
    val out =
        renderer.render(
            explanation(
                directlyChanged = true,
                rootCauses =
                    listOf(
                        RootCause(
                            "//service/a:app",
                            "Rule",
                            RootCauseKind.SELF_CHANGED,
                            0,
                            0,
                            listOf("//service/a:app")))),
            ExplainFormat.TEXT)

    assertThat(out).contains("0 hops -- this is the queried target itself")
    assertThat(out).contains("the rule's own definition or attributes changed")
  }
}
