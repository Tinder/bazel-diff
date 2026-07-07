package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.hasLength
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import assertk.assertions.messageContains
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
 * Exercises the in-process [JGitClient] against a real throwaway git repository created with the
 * `git` CLI (verifying JGit interoperates with a standard repo).
 */
class JGitClientTest : KoinTest {
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

  private fun client() = JGitClient(temp.root.toPath())

  private fun initRepoWithTwoCommits(): Pair<String, String> {
    git("init", "-q")
    git("config", "user.email", "test@example.com")
    git("config", "user.name", "test")
    File(temp.root, "file.txt").writeText("one")
    git("add", ".")
    git("commit", "-q", "-m", "first")
    val sha1 = client().resolveSha("HEAD")
    File(temp.root, "file.txt").writeText("two")
    git("add", ".")
    git("commit", "-q", "-m", "second")
    val sha2 = client().resolveSha("HEAD")
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
    val client = client()
    val file = File(temp.root, "file.txt")

    client.checkout(sha1)
    assertThat(file.readText()).isEqualTo("one")
    client.checkout(sha2)
    assertThat(file.readText()).isEqualTo("two")
  }

  @Test
  fun checkoutLeavesBranchRefsIntact() {
    val (sha1, _) = initRepoWithTwoCommits()
    val client = client()
    // Checking out a commit detaches HEAD; the branch tip must still resolve afterwards.
    client.checkout(sha1)
    val branchTip = client.resolveSha("HEAD")
    assertThat(branchTip).isEqualTo(sha1)
  }

  @Test
  fun resolveShaThrowsOnUnknownRevision() {
    initRepoWithTwoCommits()
    assertThrows(GitClientException::class.java) { client().resolveSha("no-such-ref") }
  }

  @Test
  fun fetchDoesNotThrowWithoutRemotes() {
    initRepoWithTwoCommits()
    // No remotes configured -> fetch is a no-op, must not throw.
    client().fetch()
  }

  @Test
  fun fetchWithRemoteExercisesTheRemoteLoop() {
    initRepoWithTwoCommits()
    // A self-pointing remote lets fetch() run the actual fetch-per-remote path (not just the
    // no-remote skip) without standing up a second server.
    git("remote", "add", "origin", temp.root.absolutePath)
    client().fetch()
  }

  @Test
  fun checkoutThrowsOnUnknownRevision() {
    initRepoWithTwoCommits()
    assertThrows(GitClientException::class.java) { client().checkout("does-not-exist") }
  }

  @Test
  fun resolveShaThrowsMissingRevisionForAbsentFullSha() {
    initRepoWithTwoCommits()
    // A well-formed 40-char SHA that is not in the object database. Repository.resolve() parses the
    // hex without a DB lookup, so resolveSha must detect the absence itself and flag it retryable.
    val absent = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    assertThrows(MissingRevisionException::class.java) { client().resolveSha(absent) }
  }

  @Test
  fun resolveShaThrowsWhenRevisionIsNotACommit() {
    initRepoWithTwoCommits()
    // A tree object is present but is not a commit; resolveSha must reject it as a hard error, not
    // as a retryable MissingRevisionException (a refetch would never make a tree checkoutable).
    val treeSha = git("rev-parse", "HEAD^{tree}")
    val ex = assertThrows(GitClientException::class.java) { client().resolveSha(treeSha) }
    assertThat(ex).messageContains("does not refer to a commit")
  }

  @Test
  fun resolveShaSeesCommitOnlyAfterFetch() {
    // Reproduces the reported serve failure: a commit that lands on the remote AFTER the clone is
    // absent locally, so resolveSha rejects it (rather than deferring an opaque checkout error)
    // until an on-demand fetch brings it in.
    val origin = File(temp.root, "origin").apply { mkdirs() }
    runGit(origin, "init", "-q")
    runGit(origin, "config", "user.email", "test@example.com")
    runGit(origin, "config", "user.name", "test")
    File(origin, "file.txt").writeText("one")
    runGit(origin, "add", ".")
    runGit(origin, "commit", "-q", "-m", "first")

    val workspace = File(temp.root, "workspace")
    runGit(temp.root, "clone", "-q", origin.absolutePath, workspace.absolutePath)

    // A new commit lands in origin after the clone; the workspace has not fetched it yet.
    File(origin, "file.txt").writeText("two")
    runGit(origin, "add", ".")
    runGit(origin, "commit", "-q", "-m", "second")
    val sha2 = runGit(origin, "rev-parse", "HEAD")

    val client = JGitClient(workspace.toPath())
    assertThrows(MissingRevisionException::class.java) { client.resolveSha(sha2) }
    client.fetch()
    assertThat(client.resolveSha(sha2)).isEqualTo(sha2)
  }

  @Test
  fun fetchWithoutFallbackSurfacesJGitFailure() {
    initRepoWithTwoCommits()
    // origin points at a directory that is not a git repository, so the in-process JGit fetch
    // fails. With the native fallback disabled the failure surfaces directly (legacy behaviour).
    val bogus = File(temp.root, "not-a-repo").apply { mkdirs() }
    git("remote", "add", "origin", bogus.absolutePath)
    val client = JGitClient(temp.root.toPath(), gitPath = "git", nativeFetchFallback = false)
    assertThrows(GitClientException::class.java) { client.fetch() }
  }

  @Test
  fun fetchFallbackErrorNamesNativeGitWhenBothFail() {
    initRepoWithTwoCommits()
    // JGit fetch fails (origin is not a repo) AND the native fallback fails (bogus git binary).
    // The surfaced error must name the native fallback so neither cause is hidden.
    val bogus = File(temp.root, "not-a-repo").apply { mkdirs() }
    git("remote", "add", "origin", bogus.absolutePath)
    val client =
        JGitClient(
            temp.root.toPath(), gitPath = "/nonexistent/git", nativeFetchFallback = true)
    val ex = assertThrows(GitClientException::class.java) { client.fetch() }
    assertThat(ex).messageContains("native git fallback")
  }

  @Test
  fun fetchBringsInNewCommitForShallowClone() {
    // A shallow clone is a shape JGit 5.13 may be unable to fetch (the remote's thin pack can be
    // delta-compressed against a base object below the shallow boundary). The client must fall
    // back to native git so a just-landed commit still arrives; whichever path succeeds, the end
    // state must be the new commit present and resolvable.
    val origin = File(temp.root, "origin").apply { mkdirs() }
    runGit(origin, "init", "-q")
    runGit(origin, "config", "user.email", "test@example.com")
    runGit(origin, "config", "user.name", "test")
    File(origin, "file.txt").writeText("one")
    runGit(origin, "add", ".")
    runGit(origin, "commit", "-q", "-m", "first")
    File(origin, "file.txt").writeText("two")
    runGit(origin, "add", ".")
    runGit(origin, "commit", "-q", "-m", "second")

    val workspace = File(temp.root, "workspace")
    runGit(
        temp.root,
        "clone",
        "-q",
        "--depth=1",
        "file://${origin.absolutePath}",
        workspace.absolutePath)

    // A new commit lands after the shallow clone; the workspace has not fetched it yet.
    File(origin, "file.txt").writeText("three")
    runGit(origin, "add", ".")
    runGit(origin, "commit", "-q", "-m", "third")
    val sha3 = runGit(origin, "rev-parse", "HEAD")

    val client = JGitClient(workspace.toPath())
    assertThrows(MissingRevisionException::class.java) { client.resolveSha(sha3) }
    client.fetch()
    assertThat(client.resolveSha(sha3)).isEqualTo(sha3)
  }
}
