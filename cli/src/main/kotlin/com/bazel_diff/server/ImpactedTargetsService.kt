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

  // Serializes on-demand refetches so a burst of requests for a just-landed
  // commit triggers at most one `git fetch` at a time (and skips it entirely
  // once an earlier fetch has already brought the commit in).
  private val fetchLock = Any()

  override fun getImpactedTargets(
      fromRev: String,
      toRev: String,
      targetTypes: Set<String>?
  ): ImpactedTargetsResult {
    val (fromSha, toSha) = resolveBoth(fromRev, toRev)
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
    val (fromSha, toSha) = resolveBoth(fromRev, toRev)
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

  /**
   * Resolves both revisions to commit SHAs, fetching on demand if either is missing from the local
   * clone. The service only fetches at startup, so a revision that appeared on the remote
   * afterwards would otherwise fail with a stale-clone [MissingRevisionException]. Recovery
   * escalates in two steps: a broad [GitClient.fetch] (refreshes branch/tag tips -- the common
   * "commit just landed" case), then, for a revision still missing because it is reachable from no
   * ref this clone fetches (a GitHub PR-head SHA, or a commit force-pushed off its branch), a
   * targeted [GitClient.fetchRevision] of that exact object.
   *
   * Fetches are serialized and double-checked through [fetchLock]: concurrent requests for the same
   * new commit wait on the lock, and whoever holds it re-resolves first, so an earlier fetch that
   * already made the revision resolvable saves the rest a redundant fetch. A revision still missing
   * after both steps is genuinely unknown (bad SHA, or an object the remote will not serve) and
   * propagates as [MissingRevisionException] -> HTTP 400.
   */
  private fun resolveBoth(fromRev: String, toRev: String): Pair<String, String> {
    resolveBothOrNull(fromRev, toRev)?.let {
      return it
    }
    synchronized(fetchLock) {
      // A concurrent request may have fetched while we waited on the lock; retry before fetching.
      resolveBothOrNull(fromRev, toRev)?.let {
        return it
      }
      // Broad fetch first: refreshes branch/tag tips, covering the common case of a commit that
      // landed on the remote after startup.
      logger.i { "Revision missing from local clone; fetching all refs and retrying" }
      gitClient.fetch()
      resolveBothOrNull(fromRev, toRev)?.let {
        return it
      }
      // A broad fetch only downloads objects reachable from the refs this clone's refspec covers. A
      // revision still missing is not among them -- a GitHub PR-head SHA (advertised under
      // refs/pull/*), or a commit force-pushed off its branch. Ask the remote for each such object
      // by SHA directly before giving up.
      for (rev in linkedSetOf(fromRev, toRev)) {
        if (!resolvable(rev)) {
          logger.i { "Revision '$rev' unreachable via broad fetch; fetching it directly" }
          gitClient.fetchRevision(rev)
        }
      }
    }
    // Authoritative: a revision still unresolved now is genuinely unknown and propagates as a
    // MissingRevisionException -> HTTP 400.
    return gitClient.resolveSha(fromRev) to gitClient.resolveSha(toRev)
  }

  /** Resolves both revisions, or null if either is still missing from the local clone. */
  private fun resolveBothOrNull(fromRev: String, toRev: String): Pair<String, String>? =
      try {
        gitClient.resolveSha(fromRev) to gitClient.resolveSha(toRev)
      } catch (missing: MissingRevisionException) {
        null
      }

  /** True if [rev] resolves to a commit present in the local clone. */
  private fun resolvable(rev: String): Boolean =
      try {
        gitClient.resolveSha(rev)
        true
      } catch (missing: MissingRevisionException) {
        false
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
