package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.hasLength
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isNotEqualTo
import assertk.assertions.isTrue
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import java.io.File
import org.junit.Assert.assertThrows
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule

/**
 * Exercises [ProcessGitClient] against a real throwaway git repository. `git` is a documented
 * prerequisite of bazel-diff, so it is assumed available on the test runner.
 */
class GitClientTest : KoinTest {
  @get:Rule
  val koinTestRule = KoinTestRule.create { modules(module { single<Logger> { SilentLogger } }) }

  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  /** Runs git for test setup, returning trimmed stdout; fails the test on a non-zero exit. */
  private fun git(vararg args: String): String = runGit(temp.root, *args)

  /** Like [git] but in an explicit working directory (for multi-repo fetch tests). */
  private fun runGit(dir: File, vararg args: String): String {
    val proc = ProcessBuilder(listOf("git") + args).directory(dir).redirectErrorStream(true).start()
    val output = proc.inputStream.readBytes().decodeToString()
    val code = proc.waitFor()
    check(code == 0) { "git ${args.joinToString(" ")} failed ($code): $output" }
    return output.trim()
  }

  private fun initRepoWithTwoCommits(): Pair<String, String> {
    git("init", "-q")
    git("config", "user.email", "test@example.com")
    git("config", "user.name", "test")
    File(temp.root, "file.txt").writeText("one")
    git("add", ".")
    git("commit", "-q", "-m", "first")
    val client = ProcessGitClient(temp.root.toPath())
    val sha1 = client.resolveSha("HEAD")
    File(temp.root, "file.txt").writeText("two")
    git("add", ".")
    git("commit", "-q", "-m", "second")
    val sha2 = client.resolveSha("HEAD")
    return sha1 to sha2
  }

  @Test
  fun resolveShaReturnsFullSha() {
    val (sha1, sha2) = initRepoWithTwoCommits()
    assertThat(sha1).hasLength(40)
    assertThat(sha2).hasLength(40)
    assertThat(sha1).isNotEqualTo(sha2)
  }

  @Test
  fun checkoutSwitchesWorkingTree() {
    val (sha1, sha2) = initRepoWithTwoCommits()
    val client = ProcessGitClient(temp.root.toPath())
    val file = File(temp.root, "file.txt")

    client.checkout(sha1)
    assertThat(file.readText()).isEqualTo("one")
    client.checkout(sha2)
    assertThat(file.readText()).isEqualTo("two")
  }

  @Test
  fun checkoutClearsOrphanedIndexLockAndSucceeds() {
    val (sha1, _) = initRepoWithTwoCommits()
    val client = ProcessGitClient(temp.root.toPath())
    val file = File(temp.root, "file.txt")
    val indexLock = File(temp.root, ".git/index.lock")

    // The lock a force-killed `git checkout` leaves behind: git uses O_CREAT|O_EXCL, so an existing
    // index.lock makes every checkout fail with exit 128 until it is removed. Without recovery this
    // wedges the service permanently.
    indexLock.writeText("")
    assertThat(indexLock.exists()).isTrue()

    client.checkout(sha1)

    // The stale lock was cleared and the checkout completed.
    assertThat(file.readText()).isEqualTo("one")
    assertThat(indexLock.exists()).isFalse()
  }

  @Test
  fun checkoutStillThrowsOnGenuinelyBadRevision() {
    initRepoWithTwoCommits()
    val client = ProcessGitClient(temp.root.toPath())
    // No stale lock to clear, so a real checkout failure (unknown revision) must still surface
    // rather than being swallowed by the recovery path.
    assertThrows(GitClientException::class.java) { client.checkout("no-such-revision") }
  }

  @Test
  fun resolveShaThrowsOnUnknownRevision() {
    initRepoWithTwoCommits()
    val client = ProcessGitClient(temp.root.toPath())
    assertThrows(GitClientException::class.java) { client.resolveSha("no-such-ref") }
  }

  @Test
  fun resolveShaThrowsMissingRevisionForAbsentFullSha() {
    initRepoWithTwoCommits()
    val client = ProcessGitClient(temp.root.toPath())
    // `git rev-parse <full-sha>` echoes any well-formed SHA back even when absent; resolveSha must
    // use `--verify` so a commit that isn't in the clone is rejected as a retryable miss.
    val absent = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    assertThrows(MissingRevisionException::class.java) { client.resolveSha(absent) }
  }

  @Test
  fun fetchDoesNotThrowWithoutRemotes() {
    initRepoWithTwoCommits()
    // `git fetch --all` is a no-op (exit 0) when there are no remotes, so it must not throw.
    ProcessGitClient(temp.root.toPath()).fetch()
  }

  @Test
  fun fetchRevisionBringsInACommitNoBranchReaches() {
    // A commit advertised only under refs/pull/* (a GitHub PR head) is reachable from no branch, so
    // a broad `fetch --all` never brings it in: resolveSha stays missing until fetchRevision pulls
    // that exact object. file:// forces a real pack transfer (a local-path clone would hardlink
    // every object and mask the miss).
    val origin = File(temp.root, "origin").apply { mkdirs() }
    runGit(origin, "init", "-q")
    runGit(origin, "config", "user.email", "test@example.com")
    runGit(origin, "config", "user.name", "test")
    runGit(origin, "config", "uploadpack.allowReachableSHA1InWant", "true")
    File(origin, "file.txt").writeText("one")
    runGit(origin, "add", ".")
    runGit(origin, "commit", "-q", "-m", "first")
    val branch = runGit(origin, "rev-parse", "--abbrev-ref", "HEAD")

    // A commit reachable only from refs/pull/7/head, not from any branch.
    runGit(origin, "checkout", "-q", "--detach")
    File(origin, "pr.txt").writeText("pr")
    runGit(origin, "add", ".")
    runGit(origin, "commit", "-q", "-m", "pr head")
    val prHead = runGit(origin, "rev-parse", "HEAD")
    runGit(origin, "update-ref", "refs/pull/7/head", prHead)
    // Re-attach HEAD to the branch so the clone checks out the branch, not the detached PR commit.
    runGit(origin, "checkout", "-q", branch)

    val workspace = File(temp.root, "workspace")
    runGit(temp.root, "clone", "-q", "file://${origin.absolutePath}", workspace.absolutePath)
    val client = ProcessGitClient(workspace.toPath())

    // A broad fetch does not reach the PR head; resolveSha must still report it missing.
    client.fetch()
    assertThrows(MissingRevisionException::class.java) { client.resolveSha(prHead) }

    // The targeted fetch pulls the exact object, after which it resolves.
    assertThat(client.fetchRevision(prHead)).isTrue()
    assertThat(client.resolveSha(prHead)).isEqualTo(prHead)
  }

  @Test
  fun fetchRevisionReturnsFalseForUnknownSha() {
    initRepoWithTwoCommits()
    git("remote", "add", "origin", temp.root.absolutePath)
    // A SHA no remote can supply: the best-effort targeted fetch reports failure rather than
    // throwing, leaving the authoritative miss to resolveSha.
    val absent = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    assertThat(ProcessGitClient(temp.root.toPath()).fetchRevision(absent)).isFalse()
  }

  @Test
  fun fetchRevisionReturnsFalseWithoutRemotes() {
    initRepoWithTwoCommits()
    // No remotes to ask -> nothing to fetch from -> false, and no throw.
    assertThat(ProcessGitClient(temp.root.toPath()).fetchRevision("HEAD")).isFalse()
  }
}
