package com.bazel_diff.interactor

import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.bazel.ModuleGraphParser
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.log.Logger
import com.google.common.collect.Maps
import com.google.gson.Gson
import java.io.Writer
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentMap
import java.util.stream.Collectors
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

data class TargetDistanceMetrics(val targetDistance: Int, val packageDistance: Int) {}

class CalculateImpactedTargetsInteractor : KoinComponent {
  private val gson: Gson by inject()
  private val logger: Logger by inject()
  private val moduleGraphParser = ModuleGraphParser()

  enum class ImpactType {
    DIRECT,
    INDIRECT
  }

  fun execute(
      from: Map<String, TargetHash>,
      to: Map<String, TargetHash>,
      outputWriter: Writer,
      targetTypes: Set<String>?,
      fromModuleGraphJson: String? = null,
      toModuleGraphJson: String? = null,
      excludeExternalTargets: Boolean = false,
  ) {
    /** This call might be faster if end hashes is a sorted map */
    val typeFilter = TargetTypeFilter(targetTypes, to)

    // Quick check: if module graph JSON is identical, skip module change detection entirely
    val moduleGraphChanged = fromModuleGraphJson != toModuleGraphJson

    // Detect module changes and query for impacted targets
    val changedModules = if (moduleGraphChanged) {
      detectChangedModules(fromModuleGraphJson, toModuleGraphJson)
    } else {
      emptySet()
    }

    val impactedTargets = if (changedModules.isNotEmpty()) {
      logger.i { "Module changes detected - querying for targets that depend on changed modules" }
      queryTargetsDependingOnModules(changedModules, to)
    } else {
      computeSimpleImpactedTargets(from, to)
    }

    impactedTargets
        .filter { typeFilter.accepts(it) }
        .filter { !excludeExternalTargets || !it.startsWith("//external:") }
        .sortedWith(impactedTargetOrdering(to, from))
        .let { filtered ->
          outputWriter.use { writer -> filtered.forEach { writer.write("$it\n") } }
        }
  }

  fun computeSimpleImpactedTargets(
      from: Map<String, TargetHash>,
      to: Map<String, TargetHash>
  ): Set<String> {
    val difference = Maps.difference(to, from)
    val onlyInEnd: Set<String> = difference.entriesOnlyOnLeft().keys
    val changed: Set<String> = difference.entriesDiffering().keys
    val impactedTargets =
        HashSet<String>().apply {
          addAll(onlyInEnd)
          addAll(changed)
        }
    return impactedTargets
  }

  fun executeWithDistances(
      from: Map<String, TargetHash>,
      to: Map<String, TargetHash>,
      depEdges: Map<String, List<String>>,
      outputWriter: Writer,
      targetTypes: Set<String>?,
      fromModuleGraphJson: String? = null,
      toModuleGraphJson: String? = null,
      excludeExternalTargets: Boolean = false,
  ) {
    val typeFilter = TargetTypeFilter(targetTypes, to)

    // Quick check: if module graph JSON is identical, skip module change detection entirely
    val moduleGraphChanged = fromModuleGraphJson != toModuleGraphJson

    // Detect module changes and query for impacted targets
    val changedModules = if (moduleGraphChanged) {
      detectChangedModules(fromModuleGraphJson, toModuleGraphJson)
    } else {
      emptySet()
    }

    val impactedTargets = if (changedModules.isNotEmpty()) {
      logger.i { "Module changes detected - querying for targets that depend on changed modules" }
      val moduleImpactedTargets = queryTargetsDependingOnModules(changedModules, to)
      // Mark module-impacted targets with distance 0, then compute distances from there
      val moduleImpactedHashes = from.filterKeys { !moduleImpactedTargets.contains(it) }
      computeAllDistances(moduleImpactedHashes, to, depEdges)
    } else {
      computeAllDistances(from, to, depEdges)
    }

    val ordering = impactedTargetOrdering(to, from)
    impactedTargets
        .filterKeys { typeFilter.accepts(it) }
        .filterKeys { !excludeExternalTargets || !it.startsWith("//external:") }
        .toSortedMap(ordering)
        .let { filtered ->
          outputWriter.use { writer ->
            writer.write(
                gson.toJson(
                    filtered.map {
                      mapOf(
                          "label" to it.key,
                          "targetDistance" to it.value.targetDistance,
                          "packageDistance" to it.value.packageDistance)
                    }))
          }
        }
  }

  private fun impactedTargetOrdering(
      to: Map<String, TargetHash>,
      from: Map<String, TargetHash>
  ): Comparator<String> {
    fun kindRank(label: String): Int {
      val type = to[label]?.type?.takeIf { it.isNotEmpty() }
          ?: from[label]?.type?.takeIf { it.isNotEmpty() }
      return when (type) {
        "SourceFile" -> 0
        "GeneratedFile" -> 1
        "Rule" -> 2
        null -> 4
        else -> 3
      }
    }
    return compareBy<String>({ kindRank(it) }, { it })
  }

  fun computeAllDistances(
      from: Map<String, TargetHash>,
      to: Map<String, TargetHash>,
      depEdges: Map<String, List<String>>
  ): Map<String, TargetDistanceMetrics> {
    val difference = Maps.difference(to, from)

    val newLabels = difference.entriesOnlyOnLeft().keys
    val existingImpactedLabels = difference.entriesDiffering().keys

    val impactedLabels =
        HashMap<String, ImpactType>().apply {
          newLabels.forEach { this[it] = ImpactType.DIRECT }
          existingImpactedLabels.forEach {
            this[it] =
                if (from[it]!!.directHash != to[it]!!.directHash) ImpactType.DIRECT
                else ImpactType.INDIRECT
          }
        }

    val computedResult: ConcurrentMap<String, TargetDistanceMetrics> = ConcurrentHashMap()

    impactedLabels.keys.parallelStream().forEach {
      calculateDistance(it, depEdges, computedResult, impactedLabels)
    }

    return computedResult
  }

  fun calculateDistance(
      label: String,
      depEdges: Map<String, List<String>>,
      impactedTargets: ConcurrentMap<String, TargetDistanceMetrics>,
      impactedLabels: Map<String, ImpactType>
  ): TargetDistanceMetrics {
    impactedTargets[label]?.let {
      return it
    }

    if (label !in impactedLabels) {
      throw IllegalArgumentException(
          "$label was not impacted, but was requested to calculate distance.")
    }

    // If the label is directly impacted, it has a distance of 0
    if (impactedLabels[label] == ImpactType.DIRECT) {
      return TargetDistanceMetrics(0, 0).also { impactedTargets[label] = it }
    }

    // Fix for https://github.com/Tinder/bazel-diff/issues/268: when an indirectly-impacted
    // target has either no entry in the dep-edges file, or only deps that are not in
    // `impactedLabels`, the previous behaviour was to throw InvalidDependencyEdgesException
    // and crash the whole job. The most common cause is the user filtering the hash JSON via
    // `--targetType=Rule`, which strips out SourceFile/GeneratedFile entries. When such a
    // filtered-out target is the only changed dep of an indirectly-impacted Rule, all the
    // Rule's deps from the dep-edges file (which was not filtered) are absent from the
    // impacted-labels map, so the search for an impacted predecessor turns up empty.
    //
    // Conservative fallback: log a warning that points at the most likely cause and report
    // distance 0 so the target still appears in the impacted-targets output. The user gets
    // a result instead of a crash, with enough breadcrumbs to act on the real bug if it's
    // not the targetType-filter case.
    val directDeps = depEdges[label]
    if (directDeps == null) {
      logger.w {
        "$label was indirectly impacted, but has no dependencies in the dep-edges file. " +
            "Falling back to distance 0. If you ran generate-hashes with --targetType, " +
            "note that filtering by target type drops non-Rule entries (SourceFile, " +
            "GeneratedFile) and is incompatible with distance metrics computed from a " +
            "full dep-edges file -- see https://github.com/Tinder/bazel-diff/issues/268"
      }
      return TargetDistanceMetrics(0, 0).also { impactedTargets[label] = it }
    }

    val impactedDepLabels =
        directDeps.parallelStream().filter { it in impactedLabels }.collect(Collectors.toList())
    if (impactedDepLabels.isEmpty()) {
      logger.w {
        "$label was indirectly impacted, but none of its ${directDeps.size} deps in the " +
            "dep-edges file are themselves impacted (most likely because they were filtered " +
            "out of the hash JSON via --targetType). Falling back to distance 0 -- see " +
            "https://github.com/Tinder/bazel-diff/issues/268"
      }
      return TargetDistanceMetrics(0, 0).also { impactedTargets[label] = it }
    }

    // Now compute the distance for label, which was indirectly impacted
    val (targetDistance, packageDistance) =
        impactedDepLabels
            .parallelStream()
            .map { dep ->
              val distanceMetrics =
                  calculateDistance(dep, depEdges, impactedTargets, impactedLabels)
              val crossesPackageBoundary = label.split(":")[0] != dep.split(":")[0]
              Pair(
                  distanceMetrics.targetDistance + 1,
                  distanceMetrics.packageDistance + if (crossesPackageBoundary) 1 else 0)
            }
            .collect(Collectors.toList())
            .let { distances ->
              val minTargetDistance = distances.minOf { it.first }
              val minPackageDistance = distances.minOf { it.second }
              Pair(minTargetDistance, minPackageDistance)
            }

    return TargetDistanceMetrics(targetDistance, packageDistance).also {
      impactedTargets[label] = it
    }
  }

  /**
   * Detects module changes by comparing module graphs and returns changed module keys.
   *
   * This method:
   * 1. Parses the from and to module graphs
   * 2. Identifies which modules changed (added, removed, or version changed)
   * 3. Logs the changes for visibility
   * 4. Returns the set of changed module keys
   *
   * @param fromModuleGraphJson JSON from `bazel mod graph --output=json` for starting revision
   * @param toModuleGraphJson JSON from `bazel mod graph --output=json` for final revision
   * @return Set of changed module keys, empty if no changes
   */
  private fun detectChangedModules(
      fromModuleGraphJson: String?,
      toModuleGraphJson: String?
  ): Set<String> {
    // If either module graph is missing, assume no changes
    if (fromModuleGraphJson == null || toModuleGraphJson == null) {
      return emptySet()
    }

    // Parse module graphs
    val fromGraph = moduleGraphParser.parseModuleGraph(fromModuleGraphJson)
    val toGraph = moduleGraphParser.parseModuleGraph(toModuleGraphJson)

    // Find changed modules
    val changedModules = moduleGraphParser.findChangedModules(fromGraph, toGraph)

    if (changedModules.isEmpty()) {
      logger.i { "No module changes detected" }
    } else {
      logger.i { "Detected ${changedModules.size} module changes: ${changedModules.joinToString(", ")}" }
    }

    return changedModules
  }

  /**
   * Queries Bazel to find all workspace targets that depend on any changed module.
   *
   * Maps every changed module to its matching bzlmod canonical repos, then issues a
   * single `rdeps(//..., @@a//... + @@b//... + ...)` query. Bazel executes the union
   * in one analysis pass, avoiding per-repo subprocess fan-out.
   *
   * @param changedModuleKeys Set of changed module keys (e.g., "abseil-cpp@20240722.0")
   * @param allTargets Map of all targets from the final revision
   * @return Set of target labels that are impacted by module changes
   */
  private fun queryTargetsDependingOnModules(
      changedModuleKeys: Set<String>,
      allTargets: Map<String, TargetHash>
  ): Set<String> {
    val queryService: BazelQueryService? = try {
      inject<BazelQueryService>().value
    } catch (e: Exception) {
      null
    }

    if (queryService == null) {
      logger.w { "BazelQueryService not available - cannot query for module dependencies" }
      return allTargets.keys
    }

    // Map every changed module to its matching bzlmod canonical repos. A single module
    // name can match multiple canonical repos (e.g. rules_jvm_external matches
    // rules_jvm_external~~maven~maven, rules_jvm_external~~toolchains~...). Log per
    // module so an operator can attribute a pathologically large impacted set back to
    // a specific module bump.
    val moduleRepos = mutableSetOf<String>()
    for (moduleKey in changedModuleKeys) {
      val moduleName = moduleKey.substringBefore("@")
      val matched = allTargets.keys
          .filter { it.startsWith("@@") && it.contains(moduleName) }
          .map { it.substring(2).substringBefore("//") }
      if (matched.isEmpty()) {
        logger.w { "No external repository matched module $moduleKey" }
      } else {
        logger.i { "Module $moduleKey matched ${matched.size} repos: ${matched.joinToString(", ")}" }
        moduleRepos.addAll(matched)
      }
    }

    if (moduleRepos.isEmpty()) {
      logger.i { "No external repositories matched any changed module" }
      return computeSimpleImpactedTargets(emptyMap(), allTargets)
    }

    logger.i { "Querying rdeps for ${moduleRepos.size} repositories across ${changedModuleKeys.size} changed modules" }

    val impactedTargets = mutableSetOf<String>()
    try {
      // Single unioned rdeps query: bazel executes the union in one analysis pass.
      val queryExpression = "rdeps(//..., ${moduleRepos.joinToString(" + ") { "@@$it//..." }})"
      val rdeps = runBlocking { queryService.query(queryExpression, useCquery = false) }
      val rdepLabels = rdeps.map { it.name }.filter { !it.startsWith("@@") }
      logger.i { "Found ${rdepLabels.size} workspace targets depending on changed modules" }
      impactedTargets.addAll(rdepLabels)
    } catch (e: Exception) {
      logger.e(e) { "Unioned rdeps query failed - conservatively marking all workspace targets impacted" }
      impactedTargets.addAll(allTargets.keys.filter { !it.startsWith("@@") })
    }

    impactedTargets.addAll(computeSimpleImpactedTargets(emptyMap(), allTargets))

    logger.i { "Total targets impacted by module changes: ${impactedTargets.size}" }
    return impactedTargets
  }
}
