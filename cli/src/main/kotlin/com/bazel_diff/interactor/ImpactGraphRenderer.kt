package com.bazel_diff.interactor

import com.google.gson.Gson

/** Output shapes supported by `bazel-diff explain`. */
enum class ExplainFormat {
  TEXT,
  JSON,
  DOT,
  MERMAID,
}

/**
 * Renders an [ImpactExplanation] as human-readable text, JSON, or node-link graph source.
 *
 * ## Graph conventions
 * Both graph formats draw edges in **impact-propagation** direction (root cause at the top, queried
 * target at the bottom), which is the reverse of the `--depEdgesFile` orientation. Reading downward
 * follows the change as it flows into the queried target.
 *
 * ## Color
 * Node identity is carried by the **stroke**, using two hues validated for all-pairs CVD separation
 * against both a light and a dark surface (worst pair ΔE 24.7 protan light / 26.8 dark, against a
 * ≥8 target). Fills are deliberately a uniform near-white *surface* rather than three tints: tinted
 * fills were measured at normal-vision ΔE 4.5 between the intermediate and root-cause shades, far
 * under the ≥15 floor, so they cannot carry identity. Giving every node its own light surface also
 * makes the output independent of the viewer's page background, which matters because DOT and
 * Mermaid are static source rendered somewhere we do not control.
 *
 * Color is never the sole encoding: root causes and the queried target additionally differ in
 * border shape/weight, are direct-labeled with their role, and DOT emits a legend.
 */
class ImpactGraphRenderer(private val gson: Gson) {

  private companion object {
    // Categorical slots 1 and 2 of the validated light-mode palette, plus a deliberately
    // recessive neutral for intermediates (a pass-through node is context, not a peer category).
    const val SURFACE = "#fcfcfb"
    const val INK = "#0b0b0b"
    const val INK_MUTED = "#52514e"
    const val HUE_ROOT_CAUSE = "#eb6834" // orange -- where the change originates
    const val HUE_QUERIED = "#2a78d6" // blue -- what the user asked about
    const val HUE_INTERMEDIATE = "#6b6a66" // neutral -- carries the change through
  }

  fun render(explanation: ImpactExplanation, format: ExplainFormat): String =
      when (format) {
        ExplainFormat.TEXT -> renderText(explanation)
        ExplainFormat.JSON -> gson.toJson(explanation) + "\n"
        ExplainFormat.DOT -> renderDot(explanation)
        ExplainFormat.MERMAID -> renderMermaid(explanation)
      }

  private fun renderText(e: ImpactExplanation): String = buildString {
    val type = if (e.targetType.isEmpty()) "" else "  [${e.targetType}]"
    if (!e.impacted) {
      append("${e.target}$type\n")
      append(
          if (e.removed)
              "NOT IMPACTED -- this target exists in the starting revision but not the final " +
                  "one. Deleted targets are never reported as impacted; there is nothing left " +
                  "to build.\n"
          else "NOT IMPACTED -- its hash is identical between the two revisions.\n")
      return@buildString
    }

    append("${e.target}$type\n")
    append("IMPACTED ")
    append(
        if (e.directlyChanged) "(directly -- this target changed on its own)\n"
        else "(indirectly -- the change came from its dependencies)\n")
    append("\n")

    if (e.rootCauses.isEmpty()) {
      append(
          "No root cause could be attributed. This target's hash changed but none of its deps in " +
              "the dep-edges file are impacted -- most often because the hash JSON was filtered " +
              "with --targetType while the dep-edges file was not.\n" +
              "See https://github.com/Tinder/bazel-diff/issues/268\n")
      return@buildString
    }

    append(
        if (e.truncated)
            "Root causes: ${e.totalRootCauses} (showing the ${e.rootCauses.size} nearest)\n"
        else "Root causes: ${e.totalRootCauses}\n")
    append("\n")

    e.rootCauses.forEachIndexed { index, cause ->
      val causeType = if (cause.targetType.isEmpty()) "" else "  [${cause.targetType}]"
      val reason =
          when (cause.kind) {
            RootCauseKind.NEW_TARGET -> "new target in the final revision"
            RootCauseKind.SELF_CHANGED -> selfChangedReason(cause.targetType)
          }
      append("  ${index + 1}. ${cause.label}$causeType\n")
      append("     $reason\n")
      append("     ${hops(cause.targetDistance, cause.packageHops)}\n")
      append("     ${cause.path.joinToString(" -> ")}\n")
      if (index != e.rootCauses.lastIndex) append("\n")
    }

    if (e.truncated) {
      append("\n")
      append("  ... and ${e.totalRootCauses - e.rootCauses.size} more. ")
      append("Pass --maxRootCauses=0 to list every root cause.\n")
    }
  }

  private fun selfChangedReason(targetType: String): String =
      when (targetType) {
        "SourceFile" -> "source file content changed"
        "GeneratedFile" -> "generated file changed"
        "Rule" -> "the rule's own definition or attributes changed"
        else -> "the target's own hash changed"
      }

  private fun hops(targetDistance: Int, packageHops: Int): String {
    if (targetDistance == 0) return "0 hops -- this is the queried target itself"
    val hopWord = if (targetDistance == 1) "hop" else "hops"
    val packageWord = if (packageHops == 1) "package boundary" else "package boundaries"
    return "$targetDistance $hopWord ($packageHops $packageWord crossed)"
  }

  private fun renderDot(e: ImpactExplanation): String = buildString {
    append("// bazel-diff explain -- why ${dotEscape(e.target)} was impacted\n")
    append("// Edges point in impact-propagation direction: root cause -> ... -> queried target.\n")
    append("digraph bazel_diff_impact {\n")
    append("  rankdir=TB;\n")
    append("  bgcolor=\"$SURFACE\";\n")
    append("  node [shape=box, style=\"rounded,filled\", fillcolor=\"$SURFACE\", ")
    append("fontname=\"Helvetica\", fontsize=10, fontcolor=\"$INK\", ")
    append("color=\"$HUE_INTERMEDIATE\", penwidth=1];\n")
    append("  edge [color=\"$INK_MUTED\", penwidth=1, arrowsize=0.7];\n")
    append("\n")

    val ids = e.nodes.withIndex().associate { (index, node) -> node.label to "n$index" }
    e.nodes.forEach { node ->
      val id = ids.getValue(node.label)
      append("  $id [label=\"${dotEscape(nodeLabel(node))}\"")
      when {
        node.isQueriedTarget -> append(", color=\"$HUE_QUERIED\", penwidth=2")
        node.isRootCause -> append(", color=\"$HUE_ROOT_CAUSE\", penwidth=2, peripheries=2")
      }
      append("];\n")
    }

    if (e.edges.isNotEmpty()) append("\n")
    e.edges.forEach { edge ->
      val from = ids[edge.from]
      val to = ids[edge.to]
      if (from != null && to != null) append("  $from -> $to;\n")
    }

    append("\n")
    append("  subgraph cluster_legend {\n")
    append("    label=\"Legend\"; fontname=\"Helvetica\"; fontsize=9; fontcolor=\"$INK_MUTED\";\n")
    append("    color=\"$INK_MUTED\"; penwidth=1; style=dashed;\n")
    append(
        "    lg_root [label=\"root cause\", color=\"$HUE_ROOT_CAUSE\", penwidth=2, peripheries=2];\n")
    append("    lg_mid [label=\"carries the change\"];\n")
    append("    lg_target [label=\"queried target\", color=\"$HUE_QUERIED\", penwidth=2];\n")
    append("    lg_root -> lg_mid -> lg_target [style=invis];\n")
    append("  }\n")
    append("}\n")
  }

  private fun renderMermaid(e: ImpactExplanation): String = buildString {
    append("%% bazel-diff explain -- why ${e.target} was impacted\n")
    append("%% Edges point in impact-propagation direction: root cause -> ... -> queried target.\n")
    append("flowchart TD\n")

    val ids = e.nodes.withIndex().associate { (index, node) -> node.label to "n$index" }
    e.nodes.forEach { node ->
      val id = ids.getValue(node.label)
      val text = mermaidEscape(nodeLabel(node))
      // Distinct bracket shapes so the three roles stay legible with no color at all.
      val shaped =
          when {
            node.isQueriedTarget -> "$id([\"$text\"])"
            node.isRootCause -> "$id[[\"$text\"]]"
            else -> "$id[\"$text\"]"
          }
      append("  $shaped\n")
    }

    e.edges.forEach { edge ->
      val from = ids[edge.from]
      val to = ids[edge.to]
      if (from != null && to != null) append("  $from --> $to\n")
    }

    append(
        "  classDef rootCause fill:$SURFACE,stroke:$HUE_ROOT_CAUSE,stroke-width:2px,color:$INK\n")
    append(
        "  classDef intermediate fill:$SURFACE,stroke:$HUE_INTERMEDIATE,stroke-width:1px,color:$INK\n")
    append("  classDef queried fill:$SURFACE,stroke:$HUE_QUERIED,stroke-width:2px,color:$INK\n")

    fun assign(className: String, predicate: (ExplainNode) -> Boolean) {
      val matching = e.nodes.filter(predicate).map { ids.getValue(it.label) }
      if (matching.isNotEmpty()) append("  class ${matching.joinToString(",")} $className\n")
    }
    assign("queried") { it.isQueriedTarget }
    assign("rootCause") { it.isRootCause && !it.isQueriedTarget }
    assign("intermediate") { !it.isRootCause && !it.isQueriedTarget }
  }

  /**
   * The node's caption. Root causes and the queried target are direct-labeled with their role on a
   * second line, so the graph is fully readable in monochrome or by a viewer who cannot separate
   * the two hues.
   */
  private fun nodeLabel(node: ExplainNode): String {
    val role =
        when {
          node.isQueriedTarget && node.isRootCause -> "queried target - also a root cause"
          node.isQueriedTarget -> "queried target"
          node.isRootCause -> "root cause"
          else -> null
        }
    return if (role == null) node.label else "${node.label}\n($role)"
  }

  private fun dotEscape(value: String): String =
      value.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n")

  private fun mermaidEscape(value: String): String =
      value.replace("\"", "#quot;").replace("\n", "<br/>")
}
