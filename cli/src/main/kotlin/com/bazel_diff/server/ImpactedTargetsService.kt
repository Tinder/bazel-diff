package com.bazel_diff.server

import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor
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
}

/**
 * [ImpactedTargetsProvider] that resolves both revisions, fetches their (cached) hashes via
 * [HashProvider], and reuses [CalculateImpactedTargetsInteractor] -- the exact same affectedness
 * logic the `get-impacted-targets` CLI command uses -- to compute the result.
 */
class ImpactedTargetsService(
    private val gitClient: GitClient,
    private val hashProvider: HashProvider,
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

    // Synthetic //external:* labels are not buildable in bzlmod-only mode; mirror the
    // get-impacted-targets default of excluding them when Bzlmod is enabled (issue #326).
    val excludeExternalTargets = bazelModService.isBzlmodEnabled
    val writer = StringWriter()

    val interactor = CalculateImpactedTargetsInteractor()
    val runCalc = {
      interactor.execute(
          fromData.hashes,
          toData.hashes,
          writer,
          targetTypes,
          fromData.moduleGraphJson,
          toData.moduleGraphJson,
          excludeExternalTargets)
    }

    // When the module graph changed, the interactor issues a live `rdeps` query against the
    // working tree, so it must be checked out at `to` and held there while the query runs. The
    // common (pure hash-diff) path touches no workspace state, so it runs without the lock.
    if (fromData.moduleGraphJson != toData.moduleGraphJson) {
      hashProvider.withWorkspaceAt(toSha) { runCalc() }
    } else {
      runCalc()
    }

    val impacted = writer.toString().split("\n").filter { it.isNotBlank() }
    return ImpactedTargetsResult(fromSha, toSha, impacted)
  }
}
