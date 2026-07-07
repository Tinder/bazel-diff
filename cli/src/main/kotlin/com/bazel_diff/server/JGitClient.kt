package com.bazel_diff.server

import com.bazel_diff.log.Logger
import java.io.File
import java.nio.file.Path
import org.eclipse.jgit.api.Git
import org.eclipse.jgit.errors.IncorrectObjectTypeException
import org.eclipse.jgit.errors.MissingObjectException
import org.eclipse.jgit.lib.Repository
import org.eclipse.jgit.revwalk.RevWalk
import org.eclipse.jgit.storage.file.FileRepositoryBuilder
import org.eclipse.jgit.transport.RemoteConfig
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/**
 * [GitClient] backed by JGit (pure Java), so the service performs git operations in-process without
 * forking a `git` subprocess and without requiring a `git` binary on the PATH.
 *
 * Note on "in memory": JGit removes the subprocess, but the working tree is still materialized on
 * disk — `bazel query` reads real files, so each revision must exist as files. Only the git
 * plumbing runs in-process, not the checkout itself.
 *
 * A fresh [Git]/[org.eclipse.jgit.lib.Repository] handle is opened per call and closed promptly;
 * these are cheap relative to the `bazel query` that follows, and it keeps the client stateless and
 * thread-safe (the workspace itself is still serialized by [HashService]'s lock).
 */
class JGitClient(
    private val workspacePath: Path,
    private val gitPath: String = "git",
    private val nativeFetchFallback: Boolean = true,
) : GitClient, KoinComponent {
  private val logger: Logger by inject()

  // Built lazily and only when a JGit fetch actually fails. Native git negotiates clone shapes
  // JGit 5.13 cannot fetch (shallow, partial/promisor, thin packs whose delta base is absent),
  // scoped to the same workspace and configured git binary.
  private val nativeGit: ProcessGitClient by lazy { ProcessGitClient(workspacePath, gitPath) }

  @Volatile private var warnedCloneShape = false

  private fun open(): Git {
    val repository =
        FileRepositoryBuilder()
            .setWorkTree(workspacePath.toFile())
            .readEnvironment()
            .findGitDir(workspacePath.toFile())
            .build()
    return Git(repository)
  }

  override fun fetch() {
    open().use { git ->
      warnOnUnsupportedCloneShapeOnce(git.repository)
      val remotes = RemoteConfig.getAllRemoteConfigs(git.repository.config)
      if (remotes.isEmpty()) {
        logger.i { "No git remotes configured; skipping fetch" }
        return
      }
      for (remote in remotes) {
        logger.i { "JGit fetch ${remote.name} in $workspacePath" }
        try {
          git.fetch().setRemote(remote.name).setRemoveDeletedRefs(true).call()
        } catch (e: Exception) {
          // JGit cannot fetch some clone shapes it otherwise reads fine -- notably shallow and
          // partial (blob:none) clones, where the remote's thin pack is delta-compressed against a
          // base object absent from this clone ("Missing delta base <sha>"). Native git negotiates
          // these correctly, so fall back to it rather than lame-ducking the whole service on a
          // fetch the machine is perfectly capable of.
          fetchViaNativeGitOrThrow(remote.name, e)
          return // native `git fetch --all` already covered every remote; don't re-attempt them.
        }
      }
    }
  }

  override fun fetchRevision(revision: String): Boolean {
    // Fetching an object by SHA is a negotiation JGit 5.13 does not reliably perform (it resolves
    // the ref spec against the remote's advertised refs, and a PR-head or force-pushed SHA is not
    // advertised). Route it through native git -- the same reason fetch() falls back -- which
    // supports `git fetch <remote> <sha>` against servers that allow reachable-SHA fetches.
    // With the fallback disabled the in-process engine cannot supply it, so report that honestly.
    if (!nativeFetchFallback) {
      logger.i {
        "JGit cannot fetch revision '$revision' by SHA and the native fallback is disabled"
      }
      return false
    }
    return nativeGit.fetchRevision(revision)
  }

  /**
   * Retries a failed JGit fetch with the native `git` binary (unless [nativeFetchFallback] is off,
   * in which case the original JGit failure is surfaced). If native git also fails the two failures
   * are combined so neither cause is hidden.
   */
  private fun fetchViaNativeGitOrThrow(remoteName: String, jgitError: Exception) {
    if (!nativeFetchFallback) {
      throw GitClientException("JGit fetch $remoteName failed: ${jgitError.message}", jgitError)
    }
    logger.w {
      "JGit fetch $remoteName failed (${jgitError.message}); retrying with native git ('$gitPath'). " +
          "This usually means the workspace is a shallow or partial clone, which JGit cannot fetch; " +
          "pass --gitEngine=subprocess to skip the in-process attempt entirely."
    }
    try {
      nativeGit.fetch()
    } catch (nativeError: Exception) {
      throw GitClientException(
              "JGit fetch $remoteName failed (${jgitError.message}) and the native git fallback " +
                  "('$gitPath') also failed: ${nativeError.message}",
              jgitError)
          .apply { addSuppressed(nativeError) }
    }
  }

  /**
   * Logs a one-time warning if the workspace is a shallow or partial (promisor) clone -- shapes
   * JGit 5.13 cannot fetch, so every fetch will take the native-git fallback path. Surfacing it up
   * front (rather than only on the first failed fetch) makes the misconfiguration obvious.
   */
  private fun warnOnUnsupportedCloneShapeOnce(repo: Repository) {
    if (warnedCloneShape) return
    warnedCloneShape = true
    val gitDir = repo.directory ?: return
    val shallow = File(gitDir, "shallow").exists()
    val partial =
        repo.config.getString("extensions", null, "partialClone") != null ||
            repo.config.getSubsections("remote").any {
              repo.config.getBoolean("remote", it, "promisor", false)
            }
    if (shallow || partial) {
      val kinds = listOfNotNull("shallow".takeIf { shallow }, "partial".takeIf { partial })
      logger.w {
        "workspace clone is ${kinds.joinToString("+")}; JGit cannot reliably fetch these " +
            "(thin-pack delta bases may be absent). Fetches will fall back to native git " +
            "('$gitPath'); consider --gitEngine=subprocess for this workspace."
      }
    }
  }

  override fun resolveSha(revision: String): String {
    open().use { git ->
      val objectId = git.repository.resolve(revision) ?: throw MissingRevisionException(revision)
      // Repository.resolve() turns a full 40-char SHA into an ObjectId by parsing the
      // hex WITHOUT consulting the object database, so a commit absent from this clone
      // still "resolves". Parse it through a RevWalk to confirm the object is present
      // (peeling annotated tags to their commit), so an absent commit becomes a
      // retryable MissingRevisionException here instead of an opaque "Missing unknown
      // <sha>" failure deferred to the later checkout.
      val commit =
          try {
            RevWalk(git.repository).use { walk -> walk.parseCommit(objectId) }
          } catch (e: MissingObjectException) {
            throw MissingRevisionException(revision, e)
          } catch (e: IncorrectObjectTypeException) {
            throw GitClientException("revision '$revision' does not refer to a commit", e)
          }
      return commit.name
    }
  }

  override fun checkout(revision: String) {
    open().use { git ->
      logger.i { "JGit checkout $revision in $workspacePath" }
      try {
        // Checkout by commit name leaves HEAD detached, so branch refs stay intact and a later
        // resolveSha("main") still resolves the real branch tip. setForced(true) discards any
        // conflicting working-tree state (the service only ever reads the tree, but bazel-created
        // artifacts are untracked and unaffected).
        git.checkout().setName(revision).setForced(true).call()
      } catch (e: Exception) {
        throw GitClientException("JGit checkout $revision failed: ${e.message}", e)
      }
    }
  }
}
