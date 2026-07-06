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
 * Exercises [ProcessGitClient] against a real throwaway git repository. `git` is a documented
 * prerequisite of bazel-diff, so it is assumed available on the test runner.
 */
class GitClientTest : KoinTest {
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
}
