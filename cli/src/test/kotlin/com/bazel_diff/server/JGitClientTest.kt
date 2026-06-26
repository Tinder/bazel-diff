package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.hasLength
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
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
  private fun git(vararg args: String): String {
    val proc =
        ProcessBuilder(listOf("git") + args).directory(temp.root).redirectErrorStream(true).start()
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
}
