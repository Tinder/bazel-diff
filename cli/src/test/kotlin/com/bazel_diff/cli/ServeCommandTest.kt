package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.hasLength
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import com.bazel_diff.server.GitClient
import com.bazel_diff.server.GitClientException
import com.bazel_diff.server.HashCacheStorage
import com.google.gson.GsonBuilder
import java.net.HttpURLConnection
import java.net.URL
import org.junit.After
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule

class ServeCommandTest : KoinTest {
  @get:Rule
  val koinTestRule =
      KoinTestRule.create {
        modules(
            module {
              single<Logger> { SilentLogger }
              single { GsonBuilder().disableHtmlEscaping().create() }
            })
      }

  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private val startedServers = mutableListOf<com.bazel_diff.server.BazelDiffServer>()

  @After
  fun stopServers() {
    startedServers.forEach { it.stop() }
  }

  private class FakeGitClient(private val fetchError: Exception? = null) : GitClient {
    var fetched = false

    override fun fetch() {
      fetched = true
      fetchError?.let { throw it }
    }

    override fun resolveSha(revision: String) = revision

    override fun checkout(revision: String) = Unit
  }

  private class InMemoryStorage : HashCacheStorage {
    private val entries = mutableMapOf<String, ByteArray>()

    override fun get(key: String) = entries[key]

    override fun put(key: String, data: ByteArray) {
      entries[key] = data
    }
  }

  /**
   * A serve command configured to bind an ephemeral port with a cache dir under the temp folder.
   */
  private fun command(noFetch: Boolean = false) =
      ServeCommand().apply {
        port = 0
        cacheDir = temp.newFolder().toPath()
        noInitialFetch = noFetch
      }

  private fun healthCode(server: com.bazel_diff.server.BazelDiffServer): Int {
    val conn =
        URL("http://localhost:${server.boundPort()}/health").openConnection() as HttpURLConnection
    return try {
      conn.responseCode
    } finally {
      conn.disconnect()
    }
  }

  @Test
  fun configFingerprintIsDeterministicAndShort() {
    val a = ServeCommand().computeConfigFingerprint()
    val b = ServeCommand().computeConfigFingerprint()
    assertThat(a).isEqualTo(b)
    assertThat(a).hasLength(12)
  }

  @Test
  fun configFingerprintChangesWithFlags() {
    val base = ServeCommand().computeConfigFingerprint()
    val withCquery = ServeCommand().apply { useCquery = true }.computeConfigFingerprint()
    val withRepos =
        ServeCommand()
            .apply { fineGrainedHashExternalRepos = setOf("@maven") }
            .computeConfigFingerprint()
    assertThat(withCquery).isNotEqualTo(base)
    assertThat(withRepos).isNotEqualTo(base)
  }

  @Test
  fun configFingerprintIsOrderIndependentForSets() {
    val first =
        ServeCommand()
            .apply { ignoredRuleHashingAttributes = linkedSetOf("a", "b") }
            .computeConfigFingerprint()
    val second =
        ServeCommand()
            .apply { ignoredRuleHashingAttributes = linkedSetOf("b", "a") }
            .computeConfigFingerprint()
    assertThat(first).isEqualTo(second)
  }

  @Test
  fun loadSeedFilepathsReadsNonBlankLines() {
    val cmd = command()
    assertThat(cmd.loadSeedFilepaths()).isEqualTo(emptySet())

    val seedFile = temp.newFile()
    seedFile.writeText("a/b.txt\n\n  \nc/d.txt\n")
    cmd.seedFilepaths = seedFile
    assertThat(cmd.loadSeedFilepaths().map { it.toString() }.toSet())
        .isEqualTo(setOf("a/b.txt", "c/d.txt"))
  }

  @Test
  fun buildAndStartServerBecomesHealthyAfterFetch() {
    val cmd = command()
    val git = FakeGitClient()
    val server = cmd.buildAndStartServer(git, InMemoryStorage()).also { startedServers += it }

    assertThat(git.fetched).isEqualTo(true)
    assertThat(healthCode(server)).isEqualTo(200)
  }

  @Test
  fun buildAndStartServerSkipsFetchWhenConfigured() {
    val cmd = command(noFetch = true)
    val git = FakeGitClient()
    val server = cmd.buildAndStartServer(git, InMemoryStorage()).also { startedServers += it }

    assertThat(git.fetched).isEqualTo(false)
    assertThat(healthCode(server)).isEqualTo(200)
  }

  @Test
  fun buildAndStartServerLameDucksOnFetchFailure() {
    val cmd = command()
    val git = FakeGitClient(fetchError = GitClientException("network down"))
    val server = cmd.buildAndStartServer(git, InMemoryStorage()).also { startedServers += it }

    // Fetch failed, so the instance stays un-ready and reports 503 for the load balancer to remove.
    assertThat(healthCode(server)).isEqualTo(503)
  }
}
