package com.bazel_diff.interactor

import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.bazel.Module
import com.bazel_diff.bazel.ModuleGraphParser
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
      queryTargetsDependingOnModules(changedModules, from, to)
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
      val moduleImpactedTargets = queryTargetsDependingOnModules(changedModules, from, to)
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
   * Detects module changes by comparing module graphs and returns the changed Modules.
   *
   * Resolves each changed key against the "to" graph first (the state we will query
   * against), falling back to the "from" graph for modules that were removed.
   *
   * @param fromModuleGraphJson JSON from `bazel mod graph --output=json` for the starting revision
   * @param toModuleGraphJson JSON from `bazel mod graph --output=json` for the final revision
   * @return Set of changed Modules, empty if no changes
   */
  private fun detectChangedModules(
      fromModuleGraphJson: String?,
      toModuleGraphJson: String?
  ): Set<Module> {
    if (fromModuleGraphJson == null || toModuleGraphJson == null) {
      return emptySet()
    }

    val fromGraph = moduleGraphParser.parseModuleGraph(fromModuleGraphJson)
    val toGraph = moduleGraphParser.parseModuleGraph(toModuleGraphJson)
    val changedKeys = moduleGraphParser.findChangedModules(fromGraph, toGraph)

    if (changedKeys.isEmpty()) {
      logger.i { "No module changes detected" }
      return emptySet()
    }

    val changedModules = changedKeys.mapNotNull { key -> toGraph[key] ?: fromGraph[key] }.toSet()
    logger.i { "Detected ${changedModules.size} module changes: ${changedModules.joinToString(", ") { it.key }}" }
    return changedModules
  }

  /**
   * Queries Bazel to find all targets that depend on the changed modules.
   *
   * For each changed module M we compute the set of canonical repos in `allTargets`
   * that belong to M, then run `rdeps(//..., @@<repo>//...)` once per canonical repo.
   *
   * Repo ownership is decided by two ordered predicates:
   *
   * 1. Tier A — root repo mapping. If `bazel mod dump_repo_mapping ""` maps
   *    `M.apparentName` to canonical C, any repo R with R == C or R starts with
   *    "C+" / "C~" belongs to M. Covers extension-created children (`C++ext+repo`,
   *    `C~~ext~repo`) whose parent canonical lives in M.
   * 2. Tier B — name-prefix fallback. R belongs to M if R starts with
   *    "{M.name}+" or "{M.name}~". Extension-created forms (`name++ext+repo`,
   *    `name~~ext~repo`) are already covered by these prefixes. Handles
   *    transitive modules absent from root's mapping and the case where
   *    `discoverRepoMapping` failed.
   *
   * Modules that match nothing (Tier C) are logged and skipped — a module with no
   * materialised repos in `allTargets.keys` cannot impact any hashed target, and
   * `computeSimpleImpactedTargets` still runs below to catch direct source changes.
   *
   * The key invariant vs. the previous implementation: we match on the parsed
   * canonical repo name (`label.substring(2).substringBefore("//")`), not on a
   * `contains` substring of the full label, so a module named "cpp" no longer
   * matches canonical `abseil-cpp+`.
   *
   * @param changedModules Modules identified as changed between the two graphs
   * @param from Starting-revision target hashes; used only to pick up labels whose
   *     content changed independently of any module bump
   * @param allTargets Final-revision target hashes (the set we can query against)
   * @return Set of target labels that are impacted by module changes
   */
  private fun queryTargetsDependingOnModules(
      changedModules: Set<Module>,
      from: Map<String, TargetHash>,
      allTargets: Map<String, TargetHash>
  ): Set<String> {
    return try {
      val queryService: BazelQueryService? = try {
        inject<BazelQueryService>().value
      } catch (e: Exception) {
        null
      }

      if (queryService == null) {
        logger.w { "BazelQueryService not available - cannot query for module dependencies" }
        return allTargets.keys
      }

      val repoMapping: Map<String, String> =
          try {
            runBlocking { queryService.discoverRepoMapping() }
          } catch (e: Exception) {
            logger.w { "discoverRepoMapping failed, falling back to module-name matching: ${e.message}" }
            emptyMap()
          }
      // Log size so operators can distinguish "Tier A had nothing to match
      // against" from "Tier A matched but the module wasn't in root mapping".
      // `discoverRepoMapping` returns an empty map on subprocess exit != 0
      // without throwing, so we cannot rely on the catch block above to
      // surface that case.
      logger.i { "Discovered ${repoMapping.size} root repo mapping entries" }

      // Parse `allTargets` into the set of canonical repo names once, instead of
      // rescanning every label per changed module.
      val canonicalRepos: Set<String> = allTargets.keys.asSequence()
          .filter { it.startsWith("@@") }
          .map { it.substring(2).substringBefore("//") }
          .filter { it.isNotEmpty() }
          .toSet()

      // Collect the canonical repos to query once — multiple "changed modules"
      // often collapse to the same canonical name (e.g. `findChangedModules`
      // reports both `foo@1.0` removed and `foo@2.0` added, or two modules have
      // overlapping extension-created children). Running `rdeps` per distinct
      // canonical name — not per module iteration — avoids the redundant work.
      val reposToQuery = mutableSetOf<String>()
      for (module in changedModules) {
        logger.i { "Resolving repos for changed module: ${module.name} (key: ${module.key})" }
        val moduleRepos = reposOwnedBy(module, canonicalRepos, repoMapping)
        if (moduleRepos.isEmpty()) {
          logger.w { "No external repository found for module ${module.name}" }
          continue
        }
        logger.i { "Found ${moduleRepos.size} repositories for module ${module.name}: ${moduleRepos.joinToString(", ")}" }
        reposToQuery.addAll(moduleRepos)
      }

      val impactedTargets = mutableSetOf<String>()
      for (repoName in reposToQuery) {
        try {
          val queryExpression = "rdeps(//..., @@$repoName//...)"
          logger.i { "Executing query: $queryExpression" }

          val rdeps = runBlocking { queryService.query(queryExpression, useCquery = false) }
          val rdepLabels = rdeps.map { it.name }.filter { !it.startsWith("@@") }

          logger.i { "Found ${rdepLabels.size} workspace targets depending on @@$repoName" }
          impactedTargets.addAll(rdepLabels)
        } catch (e: Exception) {
          logger.w { "Failed to query rdeps for @@$repoName: ${e.message}" }
          logger.w { "Conservatively marking all targets as impacted for this module" }
          impactedTargets.addAll(allTargets.keys.filter { !it.startsWith("@@") })
        }
      }

      // Union with hash-diff results so we still surface labels whose content changed
      // independently of any module version bump (e.g. a source file in `//app:app`
      // edited in the same commit as a MODULE.bazel update). The earlier
      // `computeSimpleImpactedTargets(emptyMap(), allTargets)` form returned every
      // key in `allTargets`, which silently defeated the rdeps filtering above.
      val directlyChanged = computeSimpleImpactedTargets(from, allTargets)
      impactedTargets.addAll(directlyChanged)

      logger.i { "Total targets impacted by module changes: ${impactedTargets.size}" }
      impactedTargets
    } catch (e: Exception) {
      logger.e(e) { "Error querying targets depending on modules" }
      allTargets.keys
    }
  }

  /**
   * Canonical repos in `canonicalRepos` that belong to `module`, using Tier A
   * (root repo mapping) plus Tier B (name-prefix fallback). See
   * [queryTargetsDependingOnModules] for the full contract.
   *
   * @param module Changed module whose owned repos we want to resolve
   * @param canonicalRepos Set of canonical repo names parsed from `allTargets.keys`
   * @param repoMapping Root module's apparent→canonical repo mapping (may be empty)
   * @return Canonical repos belonging to `module`, or empty if none matched (Tier C)
   */
  private fun reposOwnedBy(
      module: Module,
      canonicalRepos: Set<String>,
      repoMapping: Map<String, String>
  ): Set<String> {
    val prefixes = mutableListOf<String>()
    val exactMatches = mutableSetOf<String>()

    val mappedCanonical = repoMapping[module.apparentName]
    if (!mappedCanonical.isNullOrEmpty()) {
      exactMatches.add(mappedCanonical)
      prefixes.add("$mappedCanonical+")
      prefixes.add("$mappedCanonical~")
    }

    // Tier B prefixes always apply — they cover transitive modules that root's
    // `dump_repo_mapping` does not include, and also act as the sole source when
    // `discoverRepoMapping` returned empty. `++`/`~~` are subsumed by `+`/`~`
    // (extension-created repos start with `name+` or `name~` by definition).
    prefixes.add("${module.name}+")
    prefixes.add("${module.name}~")

    return canonicalRepos.filter { repo ->
      repo in exactMatches || prefixes.any { repo.startsWith(it) }
    }.toSet()
  }
}
