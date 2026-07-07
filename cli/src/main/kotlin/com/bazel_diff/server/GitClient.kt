package com.bazel_diff.server

import com.bazel_diff.log.Logger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import java.nio.file.Path
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/** Thrown when a `git` subprocess exits non-zero or produces unusable output. */
open class GitClientException(message: String, cause: Throwable? = null) :
    RuntimeException(message, cause)

/**
 * Thrown when a revision cannot be found in the local clone -- either it did not resolve at all, or
 * it resolved (a full 40-char SHA always parses to an ObjectId) but the underlying commit object is
 * absent from the object database.
 *
 * The query service treats this as *retryable*: it only fetches at startup, so a commit that landed
 * on the remote afterwards is simply not here yet. Callers refetch and retry before surfacing the
 * failure -- first a broad [GitClient.fetch], then, for a commit not reachable from any fetched ref
 * (e.g. a PR-head SHA), a targeted [GitClient.fetchRevision] (see
 * [com.bazel_diff.server.ImpactedTargetsService]).
 */
class MissingRevisionException(val revision: String, cause: Throwable? = null) :
    GitClientException("revision '$revision' is missing from the local clone", cause)

/**
 * Thin wrapper over a `git` binary (assumed on `$PATH`) scoped to a single workspace clone. The
 * query service checks out the workspace at the revisions it is asked about; see
 * [com.bazel_diff.server.HashService].
 *
 * Extracted behind an interface so services that depend on it can be unit-tested with a fake.
 */
interface GitClient {
  /** Fetches the latest refs from all remotes. Throws [GitClientException] on failure. */
  fun fetch()

  /**
   * Best-effort targeted fetch of a single [revision] (a SHA or ref name) directly from the
   * remote(s), for a commit a broad [fetch] does not bring in because it is reachable from no ref
   * this clone fetches -- e.g. a GitHub PR-head SHA (advertised only under `refs/pull/<n>/head`),
   * or a commit force-pushed off its branch. Works against servers that permit fetching a reachable
   * object by SHA (`uploadpack.allowReachableSHA1InWant`, enabled by default on GitHub).
   *
   * Returns true if a fetch reported success, false if no remote could supply it (server disallows
   * by-SHA fetch, or the revision is genuinely unknown). Never throws for an ordinary miss: callers
   * re-run [resolveSha], the authoritative check, which raises [MissingRevisionException] if the
   * object is still absent. Defaults to a no-op returning false for clients that cannot target an
   * individual revision.
   */
  fun fetchRevision(revision: String): Boolean = false

  /**
   * Resolves a revision (branch, tag, short or full SHA) to a full 40-char commit SHA, verifying
   * that the commit actually exists in the local clone. Throws [MissingRevisionException] if the
   * revision does not resolve or its object is absent (retryable via [fetch]); throws
   * [GitClientException] for other failures (e.g. the revision names a non-commit object).
   */
  fun resolveSha(revision: String): String

  /**
   * Checks out [revision], leaving the working tree at that commit. Uses `--force` so a previous
   * checkout's state never blocks the switch. Throws [GitClientException] on failure.
   */
  fun checkout(revision: String)
}

/** [GitClient] backed by the `git` binary at [gitPath], run inside [workspacePath]. */
class ProcessGitClient(
    private val workspacePath: Path,
    private val gitPath: String = "git",
) : GitClient, KoinComponent {
  private val logger: Logger by inject()

  override fun fetch() {
    logger.i { "git fetch in $workspacePath" }
    run("fetch", "--all", "--prune")
  }

  override fun fetchRevision(revision: String): Boolean {
    // A broad `fetch --all` only downloads objects reachable from the refs the refspec covers, so a
    // commit named by a SHA that no fetched ref reaches -- a GitHub PR-head commit (advertised only
    // under refs/pull/*), or a commit force-pushed off its branch -- never arrives. Ask each remote
    // for that exact object: servers that allow reachable-SHA fetches
    // (uploadpack.allowReachableSHA1InWant, GitHub's default) return it even though no local ref
    // ends up pointing at it. Best-effort per remote; a refusal or unknown SHA moves on, and the
    // authoritative miss is left to the caller's resolveSha.
    for (remote in remoteNames()) {
      try {
        logger.i { "git fetch $remote $revision in $workspacePath" }
        run("fetch", remote, revision)
        return true
      } catch (e: GitClientException) {
        logger.i { "targeted fetch of '$revision' from '$remote' failed: ${e.message}" }
      }
    }
    return false
  }

  /** Configured remote names (`git remote`); empty when none are configured or the query fails. */
  private fun remoteNames(): List<String> =
      try {
        run("remote").map(String::trim).filter(String::isNotEmpty)
      } catch (e: GitClientException) {
        logger.i { "could not list git remotes in $workspacePath: ${e.message}" }
        emptyList()
      }

  override fun resolveSha(revision: String): String {
    // `rev-parse --verify <rev>^{commit}` both resolves the revision to a commit SHA and confirms
    // that commit is present in the local clone. A bare `rev-parse <full-sha>` echoes any
    // well-formed 40-char SHA straight back without consulting the object database, so a commit
    // that isn't here would "resolve" and only fail later at checkout. A non-zero exit means the
    // revision is absent (or not a commit) locally -- retryable via a refetch.
    val output =
        try {
          run("rev-parse", "--verify", "$revision^{commit}")
        } catch (e: GitClientException) {
          throw MissingRevisionException(revision, e)
        }
    return output.firstOrNull()?.trim()?.takeIf { it.isNotEmpty() }
        ?: throw MissingRevisionException(revision)
  }

  override fun checkout(revision: String) {
    logger.i { "git checkout $revision in $workspacePath" }
    run("checkout", "--force", revision)
  }

  @OptIn(ExperimentalCoroutinesApi::class)
  private fun run(vararg args: String): List<String> {
    val cmd = mutableListOf(gitPath).apply { addAll(args) }
    val result = runBlocking {
      process(
          *cmd.toTypedArray(),
          // Merge stdout+stderr so a failure's diagnostics are captured in one place.
          stdout = Redirect.CAPTURE,
          stderr = Redirect.CAPTURE,
          workingDirectory = workspacePath.toFile(),
          destroyForcibly = true,
      )
    }
    if (result.resultCode != 0) {
      throw GitClientException(
          "git ${args.joinToString(" ")} failed (exit ${result.resultCode}): " +
              result.output.joinToString("\n"))
    }
    return result.output
  }
}
