package com.bazel_diff.server

import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor
import com.bazel_diff.interactor.ImpactedTargetWithDistance
import com.bazel_diff.log.Logger
import java.io.StringWriter
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/** Result of an impacted-targets request: the resolved SHAs and the impacted labels. */
data class ImpactedTargetsResult(
    val from: String,
    val to: String,
    val impactedTargets: List<String>,
)

/**
 * Result of an impacted-targets-with-distances request: the resolved SHAs and the impacted targets
 * each paired with their build-graph distance metrics.
 */
data class ImpactedTargetsWithDistancesResult(
    val from: String,
    val to: String,
    val impactedTargets: List<ImpactedTargetWithDistance>,
)

/**
 * Thrown when a distance query arrives but the server was started without `--trackDeps`, so no
 * dependency edges were tracked or cached. The HTTP layer maps this to a `400`.
 */
class DistancesUnavailableException(message: String) : Exception(message)

/**
 * Computes the impacted targets between two revisions. Extracted behind an interface so the HTTP
 * layer ([BazelDiffServer]) can be tested with a fake.
 */
interface ImpactedTargetsProvider {
  /**
   * @param fromRev starting revision (branch, tag, or SHA)
   * @param toRev final revision (branch, tag, or SHA)
   * @param targetTypes optional set of target types to filter (e.g. {"Rule"}); null means all.
   */
  fun getImpactedTargets(
      fromRev: String,
      toRev: String,
      targetTypes: Set<String>?
  ): ImpactedTargetsResult

  /**
   * Like [getImpactedTargets] but additionally returns each impacted target's build-graph distance
   * metrics. Throws [DistancesUnavailableException] if the server was not started with
   * `--trackDeps`.
   */
  fun getImpactedTargetsWithDistances(
      fromRev: String,
      toRev: String,
      targetTypes: Set<String>?
  ): ImpactedTargetsWithDistancesResult
}

/**
 * [ImpactedTargetsProvider] that resolves both revisions, fetches their (cached) hashes via
 * [HashProvider], and reuses [CalculateImpactedTargetsInteractor] -- the exact same affectedness
 * logic the `get-impacted-targets` CLI command uses -- to compute the result.
 */
class ImpactedTargetsService(
    private val gitClient: GitClient,
    private val hashProvider: HashProvider,
    private val depsTracked: Boolean = false,
) : ImpactedTargetsProvider, KoinComponent {
  private val bazelModService: BazelModService by inject()
  private val logger: Logger by inject()

  override fun getImpactedTargets(
      fromRev: String,
      toRev: String,
      targetTypes: Set<String>?
  ): ImpactedTargetsResult {
    val fromSha = gitClient.resolveSha(fromRev)
    val toSha = gitClient.resolveSha(toRev)
    logger.i { "Computing impacted targets $fromSha -> $toSha" }

    val fromData = hashProvider.getHashes(fromSha)
    val toData = hashProvider.getHashes(toSha)

    val writer = StringWriter()
    val interactor = CalculateImpactedTargetsInteractor()
    runOnGraph(fromData.moduleGraphJson, toData.moduleGraphJson, toSha) {
      interactor.execute(
          fromData.hashes,
          toData.hashes,
          writer,
          targetTypes,
          fromData.moduleGraphJson,
          toData.moduleGraphJson,
          excludeExternalTargets())
    }

    val impacted = writer.toString().split("\n").filter { it.isNotBlank() }
    return ImpactedTargetsResult(fromSha, toSha, impacted)
  }

  override fun getImpactedTargetsWithDistances(
      fromRev: String,
      toRev: String,
      targetTypes: Set<String>?
  ): ImpactedTargetsWithDistancesResult {
    if (!depsTracked) {
      throw DistancesUnavailableException(
          "distances unavailable: server started without --trackDeps")
    }
    val fromSha = gitClient.resolveSha(fromRev)
    val toSha = gitClient.resolveSha(toRev)
    logger.i { "Computing impacted targets with distances $fromSha -> $toSha" }

    val fromData = hashProvider.getHashes(fromSha)
    val toData = hashProvider.getHashes(toSha)

    val interactor = CalculateImpactedTargetsInteractor()
    val impacted =
        runOnGraph(fromData.moduleGraphJson, toData.moduleGraphJson, toSha) {
          // Use the `to` revision's dependency edges: distances are measured by traversing the
          // impacted labels through the final build graph (matches the CLI's depEdgesFile usage).
          interactor.computeImpactedTargetsWithDistances(
              fromData.hashes,
              toData.hashes,
              toData.depEdges,
              targetTypes,
              fromData.moduleGraphJson,
              toData.moduleGraphJson,
              excludeExternalTargets())
        }
    return ImpactedTargetsWithDistancesResult(fromSha, toSha, impacted)
  }

  // Synthetic //external:* labels are not buildable in bzlmod-only mode; mirror the
  // get-impacted-targets default of excluding them when Bzlmod is enabled (issue #326).
  private fun excludeExternalTargets(): Boolean = bazelModService.isBzlmodEnabled

  /**
   * Runs [block], holding the workspace at [toSha] only when the module graph changed. In that case
   * the interactor issues a live `rdeps` query against the working tree, so it must be checked out
   * at `to` and held there while the query runs. The common (pure hash-diff) path touches no
   * workspace state, so it runs without the lock.
   */
  private fun <T> runOnGraph(
      fromModuleGraphJson: String?,
      toModuleGraphJson: String?,
      toSha: String,
      block: () -> T
  ): T =
      if (fromModuleGraphJson != toModuleGraphJson) {
        hashProvider.withWorkspaceAt(toSha) { block() }
      } else {
        block()
      }
}
