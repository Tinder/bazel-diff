package com.bazel_diff.interactor

import com.bazel_diff.hash.TargetHash
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor.ImpactType
import com.bazel_diff.log.Logger
import java.util.ArrayDeque
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/** Why a root cause's own hash moved, independent of anything upstream of it. */
enum class RootCauseKind {
  /** The label exists in the final revision but not in the starting one. */
  NEW_TARGET,
  /** The label exists in both revisions and its own `directHash` changed. */
  SELF_CHANGED,
}

/**
 * One reason the queried target was impacted: a target whose *own* hash changed
 * ([CalculateImpactedTargetsInteractor.ImpactType.DIRECT]) and from which the change propagates
 * down to the queried target.
 *
 * [path] runs in impact-propagation order -- `[root, …, target]` -- so reading it left to right
 * follows the change as it flows downstream. It is a shortest such path; there may be others of
 * equal length, and [targetDistance] is its length in dependency hops.
 *
 * [packageHops] counts how many of those hops cross a Bazel package boundary *along [path]*.
 * Deliberately not named `packageDistance`: `/impacted_targets_with_distances` reports the
 * *minimum* package distance over all paths, which is independently minimised and can therefore be
 * smaller than the package crossings on this (target-distance-minimal) path.
 */
data class RootCause(
    val label: String,
    val targetType: String,
    val kind: RootCauseKind,
    val targetDistance: Int,
    val packageHops: Int,
    val path: List<String>,
)

/** A node of the rendered blame subgraph. */
data class ExplainNode(
    val label: String,
    val targetType: String,
    val impactType: String,
    val targetDistance: Int,
    val isRootCause: Boolean,
    val isQueriedTarget: Boolean,
)

/**
 * An edge of the rendered blame subgraph, in impact-propagation direction: [from] is a dependency
 * of [to], so a change in [from] flows into [to]. This is the reverse of the `--depEdgesFile`
 * orientation, which maps a label to the deps it consumes.
 */
data class ExplainEdge(val from: String, val to: String)

/**
 * The answer to "why was this target impacted?" -- see
 * [issue #479](https://github.com/Tinder/bazel-diff/issues/479).
 *
 * [rootCauses] is truncated to the caller's `maxRoots`; [totalRootCauses] is the count before
 * truncation so no cap is ever silently applied. [nodes]/[edges] describe only the subgraph spanned
 * by the *reported* root causes' paths.
 */
data class ImpactExplanation(
    val target: String,
    val targetType: String,
    val impacted: Boolean,
    /**
     * True when the label exists in the starting revision but not the final one. bazel-diff never
     * reports a deleted target as impacted (there is nothing left to build), so such a target is
     * reported unimpacted -- but for a *different* reason than one whose hash simply did not move,
     * and the two must not read the same.
     */
    val removed: Boolean,
    val directlyChanged: Boolean,
    val rootCauses: List<RootCause>,
    val totalRootCauses: Int,
    val truncated: Boolean,
    val nodes: List<ExplainNode>,
    val edges: List<ExplainEdge>,
)

/**
 * Thrown when the queried label appears in neither revision's hash file -- almost always a typo.
 */
class UnknownTargetException(message: String) : Exception(message)

/**
 * Attributes an impacted target to the upstream target(s) actually responsible for its hash change.
 *
 * The traversal is a breadth-first walk *up* the dependency edges from the queried target, confined
 * to labels that are themselves impacted. Every DIRECT label it reaches is a root cause, and the
 * BFS tree yields a shortest propagation path from each. Confining the walk to impacted labels is
 * what keeps it cheap on a monorepo graph: an unimpacted dep cannot have contributed to a hash
 * change, so the visited set is bounded by the impacted subgraph reachable from one target rather
 * than by the whole build graph.
 *
 * Note the walk continues *through* DIRECT labels rather than stopping at the first one. A target
 * that changed itself may also have changed deps, and both are genuine reasons the queried target
 * moved -- exactly the "service A can be triggered because of multiple reasons" case in the issue.
 */
class ExplainImpactInteractor : KoinComponent {
  private val logger: Logger by inject()

  /**
   * @param depEdges label -> its direct deps, as written by `generate-hashes --depEdgesFile`.
   * @param maxDepth stop the walk this many hops above the queried target; negative means no bound.
   *   A bounded walk can miss root causes further upstream, which is reported to the caller via the
   *   log rather than silently.
   * @param maxRoots keep at most this many root causes (nearest first); non-positive means all.
   */
  fun explain(
      from: Map<String, TargetHash>,
      to: Map<String, TargetHash>,
      depEdges: Map<String, List<String>>,
      target: String,
      maxDepth: Int = -1,
      maxRoots: Int = 0,
  ): ImpactExplanation {
    if (target !in to && target !in from) {
      throw UnknownTargetException(
          "$target is not present in either hash file. Check the label spelling, and note that " +
              "hashes generated with --targetType only contain the types you asked for.")
    }

    val targetType = typeOf(target, to, from)
    val impactedLabels = CalculateImpactedTargetsInteractor().classifyImpactedLabels(from, to)

    if (target !in impactedLabels) {
      return ImpactExplanation(
          target = target,
          targetType = targetType,
          impacted = false,
          removed = target !in to,
          directlyChanged = false,
          rootCauses = emptyList(),
          totalRootCauses = 0,
          truncated = false,
          nodes = emptyList(),
          edges = emptyList())
    }

    val (distances, parents) = walkUpstream(target, depEdges, impactedLabels, maxDepth)

    val allRoots =
        distances.keys
            .filter { impactedLabels[it] == ImpactType.DIRECT }
            .map { root ->
              val path = pathToTarget(root, parents)
              RootCause(
                  label = root,
                  targetType = typeOf(root, to, from),
                  kind = if (root in from) RootCauseKind.SELF_CHANGED else RootCauseKind.NEW_TARGET,
                  targetDistance = distances.getValue(root),
                  packageHops = path.zipWithNext().count { (a, b) -> packageOf(a) != packageOf(b) },
                  path = path)
            }
            .sortedWith(compareBy({ it.targetDistance }, { it.label }))

    val kept = if (maxRoots > 0) allRoots.take(maxRoots) else allRoots
    if (kept.size < allRoots.size) {
      logger.i {
        "Reporting ${kept.size} of ${allRoots.size} root causes for $target (--maxRootCauses); " +
            "pass --maxRootCauses=0 for all of them"
      }
    }

    val (nodes, edges) = buildSubgraph(kept, target, distances, impactedLabels, depEdges, to, from)

    return ImpactExplanation(
        target = target,
        targetType = targetType,
        impacted = true,
        removed = false,
        directlyChanged = impactedLabels[target] == ImpactType.DIRECT,
        rootCauses = kept,
        totalRootCauses = allRoots.size,
        truncated = kept.size < allRoots.size,
        nodes = nodes,
        edges = edges)
  }

  /**
   * Breadth-first walk from [target] up the dependency edges, visiting only impacted labels.
   * Returns each visited label's hop distance from [target] and its parent in the BFS tree (the
   * label it was first reached from, i.e. one hop *downstream* of it).
   */
  private fun walkUpstream(
      target: String,
      depEdges: Map<String, List<String>>,
      impactedLabels: Map<String, ImpactType>,
      maxDepth: Int,
  ): Pair<Map<String, Int>, Map<String, String>> {
    val distances = HashMap<String, Int>().apply { put(target, 0) }
    val parents = HashMap<String, String>()
    val queue = ArrayDeque<String>().apply { add(target) }
    var truncatedByDepth = false

    while (queue.isNotEmpty()) {
      val current = queue.poll()
      val depth = distances.getValue(current)
      if (maxDepth >= 0 && depth >= maxDepth) {
        if (depEdges[current].orEmpty().any { it in impactedLabels && it !in distances }) {
          truncatedByDepth = true
        }
        continue
      }
      for (dep in depEdges[current].orEmpty()) {
        if (dep !in impactedLabels || dep in distances) continue
        distances[dep] = depth + 1
        parents[dep] = current
        queue.add(dep)
      }
    }

    if (truncatedByDepth) {
      logger.w {
        "Stopped the search at --maxDepth=$maxDepth hops above $target; root causes further " +
            "upstream are not reported. Raise or drop --maxDepth for the complete attribution."
      }
    }
    return Pair(distances, parents)
  }

  /** Unrolls the BFS parent chain into a `[root, …, target]` propagation path. */
  private fun pathToTarget(root: String, parents: Map<String, String>): List<String> {
    val path = ArrayList<String>()
    var cursor: String? = root
    while (cursor != null) {
      path.add(cursor)
      cursor = parents[cursor]
    }
    return path
  }

  /**
   * Builds the subgraph spanned by the reported [roots]' paths. Nodes are exactly the labels on
   * those paths; edges are *every* dependency edge between two included nodes, not just the path
   * edges, so the rendered graph shows the real connectivity rather than a spanning tree.
   */
  private fun buildSubgraph(
      roots: List<RootCause>,
      target: String,
      distances: Map<String, Int>,
      impactedLabels: Map<String, ImpactType>,
      depEdges: Map<String, List<String>>,
      to: Map<String, TargetHash>,
      from: Map<String, TargetHash>,
  ): Pair<List<ExplainNode>, List<ExplainEdge>> {
    val rootLabels = roots.mapTo(HashSet()) { it.label }
    val included = LinkedHashSet<String>()
    roots.forEach { included.addAll(it.path) }
    included.add(target)

    val nodes =
        included
            .map { label ->
              ExplainNode(
                  label = label,
                  targetType = typeOf(label, to, from),
                  impactType = impactedLabels.getValue(label).name.lowercase(),
                  targetDistance = distances.getValue(label),
                  isRootCause = label in rootLabels,
                  isQueriedTarget = label == target)
            }
            .sortedWith(compareByDescending<ExplainNode> { it.targetDistance }.thenBy { it.label })

    val edges =
        included
            .flatMap { consumer ->
              depEdges[consumer]
                  .orEmpty()
                  .filter { it in included }
                  .map { dep -> ExplainEdge(from = dep, to = consumer) }
            }
            .distinct()
            .sortedWith(compareBy({ it.from }, { it.to }))

    return Pair(nodes, edges)
  }

  /**
   * The target's declared type, preferring the final revision. Empty when types were not hashed.
   */
  private fun typeOf(
      label: String,
      to: Map<String, TargetHash>,
      from: Map<String, TargetHash>
  ): String =
      to[label]?.type?.takeIf { it.isNotEmpty() }
          ?: from[label]?.type?.takeIf { it.isNotEmpty() }
          ?: ""

  /** The package part of a label, i.e. everything before the `:`. */
  private fun packageOf(label: String): String = label.substringBefore(":")
}
