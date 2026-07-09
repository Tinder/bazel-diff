package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import java.util.Calendar
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

class BazelClient(
    private val useCquery: Boolean,
    private val cqueryExpression: String?,
    private val fineGrainedHashExternalRepos: Set<String>,
    private val excludeExternalTargets: Boolean,
    private val excludeTargetsQuery: String?,
) : KoinComponent {
  private val logger: Logger by inject()
  private val queryService: BazelQueryService by inject()
  private val bazelModService: BazelModService by inject()

  /**
   * Latched once a query proves `//external` is unavailable in this workspace (bzlmod-only mode
   * where the `bazel mod graph` probe in [BazelModService] returned a false negative). Subsequent
   * [queryAllTargets] calls -- e.g. later revisions served by a long-running `serve` process --
   * then skip `//external:all-targets` up front rather than failing and retrying every time.
   */
  @Volatile private var externalPackageUnavailable = false

  /**
   * Wraps [query] so that every target matching the user-supplied [excludeTargetsQuery] is removed
   * from the result set via Bazel's `except` operator. Returns [query] unchanged when no exclude
   * query is configured. This is how callers drop, e.g., `manual`-tagged targets from the graph
   * (issue #392): `--excludeTargetsQuery='attr("tags", "[\[ ]manual[,\]]", //...)'`. Excluded
   * targets are simply absent from hashing; any kept target that depended on one logs an "Unable to
   * calculate digest" warning and hashes without that input, which is the intended behavior for
   * leaf targets like `manual`-tagged tests/binaries.
   */
  private fun withExcludeFilter(query: String): String =
      excludeTargetsQuery?.takeIf { it.isNotBlank() }?.let { "($query) except ($it)" } ?: query

  suspend fun queryAllTargets(): List<BazelTarget> {
    val queryEpoch = Calendar.getInstance().getTimeInMillis()

    // Skip //external:all-targets when explicitly excluded, when Bzlmod is enabled (//external not
    // available), or when an earlier query already proved //external is unavailable here (the
    // Bzlmod probe false-negatived -- see [externalPackageUnavailable]).
    val repoTargetsQuery =
        if (excludeExternalTargets ||
            bazelModService.isBzlmodEnabled ||
            externalPackageUnavailable) {
          emptyList()
        } else {
          listOf("//external:all-targets")
        }

    // When Bzlmod is enabled and Bazel 8.6.0+ is available, query bzlmod-managed external repo
    // definitions via `bazel mod show_repo --output=streamed_proto`. This creates synthetic
    // //external:* targets from Build.Repository protos, enabling fine-grained hashing of
    // bzlmod dependencies.
    val bzlmodRepoTargets =
        if (bazelModService.isBzlmodEnabled && queryService.canUseBzlmodShowRepo) {
          logger.i { "Querying bzlmod-managed external repos via mod show_repo" }
          queryService.queryBzlmodRepos()
        } else {
          emptyList()
        }

    val targets =
        if (useCquery) {
          // Explicitly listing external repos here sometimes causes issues mentioned at
          // https://bazel.build/query/cquery#recursive-target-patterns. Hence, we query all
          // dependencies with `deps`
          // instead. However, we still need to append all "//external:*" targets because
          // fine-grained hash
          // computation depends on hashing of source files in external repos as well, which is
          // limited to repos
          // explicitly mentioned in `fineGrainedHashExternalRepos` flag. Therefore, for any repos
          // not mentioned there
          // we are still relying on the repo-generation target under `//external` to compute the
          // hash.
          //
          // In addition, we must include all source dependencies in this query in order for them to
          // show up in
          // `configuredRuleInput`. Hence, one must not filter them out with `kind(rule, deps(..))`.
          val expression = withExcludeFilter(cqueryExpression ?: "deps(//...:all-targets)")
          val mainTargets = queryService.query(expression, useCquery = true)
          val repoTargets =
              if (repoTargetsQuery.isNotEmpty()) {
                try {
                  queryService.query(repoTargetsQuery.joinToString(" + ") { "'$it'" })
                } catch (e: ExternalPackageUnavailableException) {
                  // //external isn't queryable here despite the Bzlmod probe saying otherwise; the
                  // main deps() query already succeeded, so just drop the external repo targets.
                  noteExternalPackageUnavailable()
                  emptyList()
                }
              } else {
                emptyList()
              }
          (mainTargets + repoTargets).distinctBy { it.name }
        } else {
          val buildTargetsQuery =
              listOf("//...:all-targets") +
                  fineGrainedHashExternalRepos.map { "$it//...:all-targets" }
          try {
            queryService.query(
                withExcludeFilter(
                    (repoTargetsQuery + buildTargetsQuery).joinToString(" + ") { "'$it'" }))
          } catch (e: ExternalPackageUnavailableException) {
            // The combined query failed because //external is unavailable. If we hadn't asked for
            // it, the error came from elsewhere and must not be masked; otherwise retry without it.
            if (repoTargetsQuery.isEmpty()) throw e
            noteExternalPackageUnavailable()
            queryService.query(withExcludeFilter(buildTargetsQuery.joinToString(" + ") { "'$it'" }))
          }
        }
    val allTargets = (targets + bzlmodRepoTargets).distinctBy { it.name }
    val queryDuration = Calendar.getInstance().getTimeInMillis() - queryEpoch
    logger.i { "All targets queried in $queryDuration" }
    return allTargets
  }

  /**
   * Latches [externalPackageUnavailable] the first time a query proves `//external` is unqueryable,
   * emitting a one-time explanation. This means the Bzlmod probe (`bazel mod graph`) reported that
   * Bzlmod was off when it is actually on -- common in restricted environments where `bazel mod
   * graph` cannot resolve the module graph. Dropping `//external:all-targets` is correct here: when
   * the package genuinely isn't available, there are no WORKSPACE-defined external targets to hash.
   */
  private fun noteExternalPackageUnavailable() {
    if (!externalPackageUnavailable) {
      externalPackageUnavailable = true
      logger.w {
        "//external is not available in this workspace (WORKSPACE deprecated / bzlmod-only), but " +
            "Bzlmod auto-detection did not catch it; dropping //external:all-targets from the " +
            "query and continuing. Pass --excludeExternalTargets to skip it up front and silence " +
            "this."
      }
    }
  }
}
