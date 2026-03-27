package com.bazel_diff.interactor

import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.bazel.ModuleGraphParser
import com.bazel_diff.bazel.ModuleLockFileParser
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.log.Logger
import com.google.common.annotations.VisibleForTesting
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
  private val moduleLockFileParser = ModuleLockFileParser()

  @VisibleForTesting class InvalidDependencyEdgesException(message: String) : Exception(message)

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
      fromModuleLockFileJson: String? = null,
      toModuleLockFileJson: String? = null
  ) {
    /** This call might be faster if end hashes is a sorted map */
    val typeFilter = TargetTypeFilter(targetTypes, to)

    val impactedTargets = detectExternallyImpactedTargets(
        from, to, fromModuleGraphJson, toModuleGraphJson,
        fromModuleLockFileJson, toModuleLockFileJson)
        ?: computeSimpleImpactedTargets(from, to)

    impactedTargets
        .filter { typeFilter.accepts(it) }
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
      fromModuleLockFileJson: String? = null,
      toModuleLockFileJson: String? = null
  ) {
    val typeFilter = TargetTypeFilter(targetTypes, to)

    val externallyImpactedTargets = detectExternallyImpactedTargets(
        from, to, fromModuleGraphJson, toModuleGraphJson,
        fromModuleLockFileJson, toModuleLockFileJson)

    val impactedTargets = if (externallyImpactedTargets != null) {
      // Module/extension change path: mark externally-impacted targets as if removed from `from`
      // so computeAllDistances sees them as changed and computes distance 0
      val reducedFrom = from.filterKeys { !externallyImpactedTargets.contains(it) }
      computeAllDistances(reducedFrom, to, depEdges)
    } else {
      computeAllDistances(from, to, depEdges)
    }

    impactedTargets
        .filterKeys { typeFilter.accepts(it) }
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

    val directDeps =
        depEdges[label]
            ?: throw InvalidDependencyEdgesException(
                "$label was indirectly impacted, but has no dependencies.")

    // Now compute the distance for label, which was indirectly impacted
    val (targetDistance, packageDistance) =
        directDeps
            .parallelStream()
            .filter { it in impactedLabels }
            .collect(Collectors.toList())
            .let { impactedDepLabels ->
              if (impactedDepLabels.isEmpty()) {
                throw InvalidDependencyEdgesException(
                    "$label was indirectly impacted, but has no impacted dependencies.")
              }
              impactedDepLabels.parallelStream()
            }
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
   * Queries Bazel to find all targets that depend on the changed modules.
   *
   * This uses an efficient module-level query approach:
   * 1. Identifies which external repository each changed module corresponds to
   * 2. Uses `rdeps(//..., @@module~version//...)` to find workspace targets depending on each module
   * 3. Returns the union of all impacted targets
   *
   * @param changedModuleKeys Set of changed module keys (e.g., "abseil-cpp@20240722.0")
   * @param allTargets Map of all targets from the final revision
   * @return Set of target labels that are impacted by module changes
   */
  private fun queryTargetsDependingOnModules(
      changedModuleKeys: Set<String>,
      allTargets: Map<String, TargetHash>
  ): Set<String> {
    return try {
      // Inject BazelQueryService if available
      val queryService: BazelQueryService? = try {
        inject<BazelQueryService>().value
      } catch (e: Exception) {
        null
      }

      if (queryService == null) {
        logger.w { "BazelQueryService not available - cannot query for module dependencies" }
        return allTargets.keys
      }

      val impactedTargets = mutableSetOf<String>()

      for (moduleKey in changedModuleKeys) {
        // Extract module name from key (e.g., "abseil-cpp" from "abseil-cpp@20240722.0")
        val moduleName = moduleKey.substringBefore("@")
        logger.i { "Querying targets depending on module: $moduleName (key: $moduleKey)" }

        // Find the canonical repository name for this module from allTargets
        // Bzlmod repos look like: @@abseil-cpp~20240116.2//... or @@rules_jvm_external~~maven~maven//...
        val moduleRepos = allTargets.keys
            .filter { it.startsWith("@@") && it.contains(moduleName) }
            .map { it.substring(2).substringBefore("//") } // Extract repo name
            .toSet()

        if (moduleRepos.isEmpty()) {
          logger.w { "No external repository found for module $moduleName" }
          continue
        }

        logger.i { "Found ${moduleRepos.size} repositories for module $moduleName: ${moduleRepos.joinToString(", ")}" }

        // Query workspace targets that depend on any target in the changed module repo
        for (repoName in moduleRepos) {
          try {
            // Use rdeps to find all workspace targets depending on this module
            // rdeps(universe, target_set) finds all targets in universe that depend on target_set
            val queryExpression = "rdeps(//..., @@$repoName//...)"
            logger.i { "Executing query: $queryExpression" }

            val rdeps = runBlocking { queryService.query(queryExpression, useCquery = false) }
            val rdepLabels = rdeps.map { it.name }.filter { !it.startsWith("@@") } // Filter to workspace targets only

            logger.i { "Found ${rdepLabels.size} workspace targets depending on @@$repoName" }
            impactedTargets.addAll(rdepLabels)
          } catch (e: Exception) {
            logger.w { "Failed to query rdeps for @@$repoName: ${e.message}" }
            logger.w { "Conservatively marking all targets as impacted for this module" }
            // On error for this module, add all workspace targets
            impactedTargets.addAll(allTargets.keys.filter { !it.startsWith("@@") })
          }
        }
      }

      logger.i { "Total targets impacted by module changes: ${impactedTargets.size}" }
      impactedTargets
    } catch (e: Exception) {
      logger.e(e) { "Error querying targets depending on modules" }
      // On error, conservatively mark all targets as impacted
      allTargets.keys
    }
  }

  /**
   * Detects impacted targets from module graph or lock file changes.
   *
   * Returns null if no external changes were detected (caller should fall back to hash diffing).
   * Returns a Set<String> of impacted target labels if module graph or generatedRepoSpecs changed.
   */
  private fun detectExternallyImpactedTargets(
      from: Map<String, TargetHash>,
      to: Map<String, TargetHash>,
      fromModuleGraphJson: String?,
      toModuleGraphJson: String?,
      fromModuleLockFileJson: String?,
      toModuleLockFileJson: String?
  ): Set<String>? {
    val changedModules = if (fromModuleGraphJson != toModuleGraphJson) {
      detectChangedModules(fromModuleGraphJson, toModuleGraphJson)
    } else {
      emptySet()
    }

    if (changedModules.isNotEmpty()) {
      logger.i { "Module changes detected - querying for targets that depend on changed modules" }
      val rdepTargets = queryTargetsDependingOnModules(changedModules, to)
      return rdepTargets + computeSimpleImpactedTargets(from, to)
    }

    val changedRepos = if (fromModuleLockFileJson != toModuleLockFileJson) {
      detectChangedRepos(fromModuleLockFileJson, toModuleLockFileJson)
    } else {
      emptySet()
    }

    if (changedRepos.isNotEmpty()) {
      logger.i { "Module extension repo changes detected - querying for targets that depend on changed repos" }
      val rdepTargets = queryTargetsDependingOnRepos(changedRepos, to)
      return rdepTargets + computeSimpleImpactedTargets(from, to)
    }

    return null
  }

  private fun detectChangedRepos(fromLockJson: String?, toLockJson: String?): Set<String> {
    if (fromLockJson == null || toLockJson == null) {
      logger.i { "MODULE.bazel.lock missing for one or both revisions — skipping lock file diff" }
      return emptySet()
    }
    val oldSpecs = moduleLockFileParser.parseGeneratedRepoSpecs(fromLockJson)
    val newSpecs = moduleLockFileParser.parseGeneratedRepoSpecs(toLockJson)
    val changed = moduleLockFileParser.findChangedRepos(oldSpecs, newSpecs)
    if (changed.isEmpty()) {
      logger.i { "No module extension repo changes detected" }
    } else {
      logger.i { "Detected ${changed.size} changed extension repos: ${changed.joinToString(", ")}" }
    }
    return changed
  }

  private fun queryTargetsDependingOnRepos(
      changedCanonicalRepoNames: Set<String>,
      allTargets: Map<String, TargetHash>
  ): Set<String> {
    return try {
      val queryService: BazelQueryService? = try {
        inject<BazelQueryService>().value
      } catch (e: Exception) {
        null
      }

      if (queryService == null) {
        logger.w { "BazelQueryService not available - cannot query for repo dependencies" }
        return allTargets.keys
      }

      val impactedTargets = mutableSetOf<String>()

      for (repoName in changedCanonicalRepoNames) {
        try {
          val queryExpression = "rdeps(//..., @@$repoName//...)"
          logger.i { "Executing query: $queryExpression" }
          val rdeps = runBlocking { queryService.query(queryExpression, useCquery = false) }
          val rdepLabels = rdeps.map { it.name }.filter { !it.startsWith("@@") }
          logger.i { "Found ${rdepLabels.size} workspace targets depending on @@$repoName" }
          impactedTargets.addAll(rdepLabels)
        } catch (e: Exception) {
          logger.w { "Failed to query rdeps for @@$repoName: ${e.message}" }
          impactedTargets.addAll(allTargets.keys.filter { !it.startsWith("@@") })
        }
      }

      logger.i { "Total targets impacted by extension repo changes: ${impactedTargets.size}" }
      impactedTargets
    } catch (e: Exception) {
      logger.e(e) { "Error querying targets depending on repos" }
      allTargets.keys
    }
  }
}
