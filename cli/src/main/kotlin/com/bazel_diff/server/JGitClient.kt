package com.bazel_diff.server

import com.bazel_diff.log.Logger
import java.nio.file.Path
import org.eclipse.jgit.api.Git
import org.eclipse.jgit.errors.IncorrectObjectTypeException
import org.eclipse.jgit.errors.MissingObjectException
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
class JGitClient(private val workspacePath: Path) : GitClient, KoinComponent {
  private val logger: Logger by inject()

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
          throw GitClientException("JGit fetch ${remote.name} failed: ${e.message}", e)
        }
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
