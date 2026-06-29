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
class GitClientException(message: String, cause: Throwable? = null) :
    RuntimeException(message, cause)

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
   * Resolves a revision (branch, tag, short or full SHA) to a full 40-char commit SHA. Throws
   * [GitClientException] if the revision cannot be resolved.
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

  override fun resolveSha(revision: String): String {
    val output = run("rev-parse", revision)
    return output.firstOrNull()?.trim()?.takeIf { it.isNotEmpty() }
        ?: throw GitClientException("git rev-parse $revision produced no output")
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
