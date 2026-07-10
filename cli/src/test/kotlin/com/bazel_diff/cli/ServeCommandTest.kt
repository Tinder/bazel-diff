package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.hasLength
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isInstanceOf
import assertk.assertions.isNotEqualTo
import assertk.assertions.isNotNull
import assertk.assertions.isNull
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import com.bazel_diff.server.GitClient
import com.bazel_diff.server.GitClientException
import com.bazel_diff.server.HashCacheStorage
import com.bazel_diff.server.LocalDiskHashCacheStorage
import com.bazel_diff.server.ProcessGitClient
import com.bazel_diff.server.ServerMetrics
import com.google.gson.Gson
import com.google.gson.GsonBuilder
import java.net.HttpURLConnection
import java.net.URL
import java.time.Duration
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

  /** Records which SHAs warmup asked for and optionally fails for a configured subset. */
  private class RecordingHashProvider(private val failFor: Set<String> = emptySet()) :
      com.bazel_diff.server.HashProvider {
    val warmed = mutableListOf<String>()

    override fun getHashes(sha: String): com.bazel_diff.interactor.HashFileData {
      warmed += sha
      if (sha in failFor) throw RuntimeException("boom for $sha")
      return com.bazel_diff.interactor.HashFileData(emptyMap(), null)
    }

    override fun <T> withWorkspaceAt(sha: String, block: () -> T): T = block()
  }

  private object NoopImpactedTargets : com.bazel_diff.server.ImpactedTargetsProvider {
    override fun getImpactedTargets(fromRev: String, toRev: String, targetTypes: Set<String>?) =
        com.bazel_diff.server.ImpactedTargetsResult(fromRev, toRev, emptyList())

    override fun getImpactedTargetsWithDistances(
        fromRev: String,
        toRev: String,
        targetTypes: Set<String>?
    ) = com.bazel_diff.server.ImpactedTargetsWithDistancesResult(fromRev, toRev, emptyList())
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
    val withExcludeQuery =
        ServeCommand()
            .apply { excludeTargetsQuery = "attr(\"tags\", \"manual\", //...)" }
            .computeConfigFingerprint()
    assertThat(withCquery).isNotEqualTo(base)
    assertThat(withRepos).isNotEqualTo(base)
    assertThat(withExcludeQuery).isNotEqualTo(base)
  }

  @Test
  fun configFingerprintChangesWithTrackDeps() {
    // A deps-tracking server must never reuse deps-less cache entries, so the flag must affect the
    // cache key.
    val base = ServeCommand().computeConfigFingerprint()
    val withTrackDeps = ServeCommand().apply { trackDeps = true }.computeConfigFingerprint()
    assertThat(withTrackDeps).isNotEqualTo(base)
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
  fun createGitClientUsesSubprocessEngine() {
    val cmd = command().apply { workspacePath = temp.newFolder().toPath() }
    assertThat(cmd.createGitClient()).isInstanceOf(ProcessGitClient::class)
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
  fun warmUpCacheGeneratesEachRevisionAndIsBestEffort() {
    val cmd = command().apply { warmupRevisions = linkedSetOf("main", "release") }
    val git = FakeGitClient() // resolveSha is identity, so warmup SHAs == revision names
    val provider = RecordingHashProvider(failFor = setOf("release"))

    // A failing revision must not throw or stop the others from warming.
    cmd.warmUpCache(git, provider)

    assertThat(provider.warmed).isEqualTo(listOf("main", "release"))
  }

  @Test
  fun warmupFailureStillBecomesReady() {
    val cmd = command().apply { warmupRevisions = linkedSetOf("bogus") }
    val git = FakeGitClient()
    val provider = RecordingHashProvider(failFor = setOf("bogus"))
    val ready = java.util.concurrent.atomic.AtomicBoolean(false)
    val server =
        com.bazel_diff.server
            .BazelDiffServer(0, NoopImpactedTargets) { ready.get() }
            .also { startedServers += it }
    server.start()

    cmd.performInitialFetch(git, provider, ready, server)

    assertThat(ready.get()).isEqualTo(true)
    assertThat(healthCode(server)).isEqualTo(200)
  }

  @Test
  fun parsesCachePruningFlagsIncludingTheIntervalDefault() {
    val cmd = ServeCommand()
    picocli
        .CommandLine(cmd)
        .parseArgs(
            "--workspacePath",
            "/tmp/ws",
            "--cacheDir",
            "/tmp/cache",
            "--cacheMaxAge",
            "7d",
            "--cacheMaxSize",
            "10gb",
            "--cacheMaxEntries",
            "500")

    assertThat(cmd.cacheMaxAge).isEqualTo(Duration.ofDays(7))
    assertThat(cmd.cacheMaxSize).isEqualTo(10L * 1024 * 1024 * 1024)
    assertThat(cmd.cacheMaxEntries).isEqualTo(500)
    // The default is a duration string parsed through the same converter.
    assertThat(cmd.cachePruneInterval).isEqualTo(Duration.ofHours(1))
  }

  @Test
  fun cachePruneLimitsReflectsFlags() {
    val cmd = ServeCommand()
    assertThat(cmd.cachePruneLimits().hasAny).isFalse()

    cmd.cacheMaxAge = Duration.ofDays(7)
    cmd.cacheMaxEntries = 100
    cmd.cacheMaxSize = 1024L
    val limits = cmd.cachePruneLimits()

    assertThat(limits.maxAge).isEqualTo(Duration.ofDays(7))
    assertThat(limits.maxEntries).isEqualTo(100)
    assertThat(limits.maxBytes).isEqualTo(1024L)
  }

  @Test
  fun buildCachePrunerIsNullWhenNoLimitsAreSet() {
    assertThat(command().buildCachePruner(InMemoryStorage())).isNull()
  }

  @Test
  fun buildCachePrunerIsNullWhenTheBackendCannotPrune() {
    // A limit is set, but InMemoryStorage is not a PrunableHashCacheStorage: ignored (with a
    // warning)
    // rather than silently pretending the cache is bounded.
    val cmd = command().apply { cacheMaxEntries = 10 }
    assertThat(cmd.buildCachePruner(InMemoryStorage())).isNull()
  }

  @Test
  fun buildCachePrunerIsBuiltForAPrunableBackendWithLimits() {
    val cmd = command().apply { cacheMaxEntries = 10 }
    val storage = LocalDiskHashCacheStorage(temp.newFolder().toPath())
    assertThat(cmd.buildCachePruner(storage)).isNotNull()
  }

  @Test
  fun buildAndStartServerLameDucksOnFetchFailure() {
    val cmd = command()
    val git = FakeGitClient(fetchError = GitClientException("network down"))
    val server = cmd.buildAndStartServer(git, InMemoryStorage()).also { startedServers += it }

    // Fetch failed, so the instance stays un-ready and reports 503 for the load balancer to remove.
    assertThat(healthCode(server)).isEqualTo(503)
  }

  @Test
  fun metricsEndpointReportsInstanceAndCacheUsage() {
    val cmd = command()
    // Back the service with a real on-disk cache under the configured --cacheDir so the endpoint
    // reports live size usage through the full ServeCommand -> BazelDiffServer -> storage wiring.
    val storage = LocalDiskHashCacheStorage(cmd.cacheDir)
    storage.put("k", ByteArray(64))
    val server = cmd.buildAndStartServer(FakeGitClient(), storage).also { startedServers += it }

    val conn =
        URL("http://localhost:${server.boundPort()}/metrics").openConnection() as HttpURLConnection
    try {
      assertThat(conn.responseCode).isEqualTo(200)
      val parsed =
          Gson().fromJson(conn.inputStream.bufferedReader().readText(), ServerMetrics::class.java)
      assertThat(parsed.cache.directory).isEqualTo(cmd.cacheDir.toString())
      assertThat(parsed.cache.entries).isEqualTo(1L)
      assertThat(parsed.cache.sizeBytes).isEqualTo(64L)
      assertThat(parsed.ready).isEqualTo(true)
    } finally {
      conn.disconnect()
    }
  }
}
