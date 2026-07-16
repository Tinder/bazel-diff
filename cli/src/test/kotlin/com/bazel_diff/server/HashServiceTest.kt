package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.doesNotContain
import assertk.assertions.hasSize
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import assertk.assertions.isNull
import assertk.assertions.startsWith
import com.bazel_diff.SilentLogger
import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.log.Logger
import com.google.gson.GsonBuilder
import java.nio.file.Path
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.mockito.Mockito.times
import org.mockito.Mockito.verify
import org.mockito.junit.MockitoJUnit
import org.mockito.kotlin.any
import org.mockito.kotlin.anyOrNull
import org.mockito.kotlin.eq
import org.mockito.kotlin.mock
import org.mockito.kotlin.whenever

class HashServiceTest : KoinTest {
  @get:Rule val mockitoRule = MockitoJUnit.rule()

  private val buildGraphHasher: BuildGraphHasher = mock()
  private val bazelModService: BazelModService = mock()

  @get:Rule
  val koinTestRule =
      KoinTestRule.create {
        modules(
            module {
              single<Logger> { SilentLogger }
              single { GsonBuilder().disableHtmlEscaping().create() }
              single { buildGraphHasher }
              single { bazelModService }
            })
      }

  /** Records git operations so the test can assert on how many checkouts happened. */
  private class RecordingGitClient : GitClient {
    val checkouts = mutableListOf<String>()

    override fun fetch() = Unit

    override fun resolveSha(revision: String) = revision

    override fun checkout(revision: String) {
      checkouts += revision
    }
  }

  private class InMemoryStorage : HashCacheStorage {
    val entries = mutableMapOf<String, ByteArray>()

    override fun get(key: String): ByteArray? = entries[key]

    override fun put(key: String, data: ByteArray) {
      entries[key] = data
    }
  }

  private val sampleHashes = mapOf("//:a" to TargetHash("Rule", "h", "d"))

  private val sampleHashesWithDeps =
      mapOf(
          "//a:lib" to TargetHash("Rule", "ha", "da", deps = listOf("//b:lib")),
          "//b:lib" to TargetHash("Rule", "hb", "db", deps = emptyList()))

  private fun newService(git: GitClient, storage: HashCacheStorage, trackDeps: Boolean = false) =
      HashService(git, storage, "fp", emptySet(), emptySet(), trackDeps)

  @Test
  fun cacheMissGeneratesAndStores() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull()))
        .thenReturn(sampleHashes)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn(null) }
    val git = RecordingGitClient()
    val storage = InMemoryStorage()

    val data = newService(git, storage).getHashes("sha1")

    assertThat(data.hashes).isEqualTo(sampleHashes)
    assertThat(data.moduleGraphJson).isNull()
    assertThat(git.checkouts).isEqualTo(listOf("sha1"))
    assertThat(storage.entries).hasSize(1)
    // The cache key combines the SHA and the config fingerprint.
    assertThat(storage.entries.keys.single()).isEqualTo("sha1.fp")
  }

  @Test
  fun scopedRequestUsesADistinctCacheKeyAndPassesTheSetToTheHasher() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull()))
        .thenReturn(sampleHashes)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn(null) }
    val storage = InMemoryStorage()
    val modified = setOf(Path.of("pkg/A.kt"), Path.of("pkg/B.kt"))

    newService(RecordingGitClient(), storage).getHashes("sha1", modified)

    // A content-scoped entry is keyed <sha>.<fingerprint>.<digest>, so it never mixes with (or is
    // served in place of) the full-content `sha1.fp` entry.
    val key = storage.entries.keys.single()
    assertThat(key).startsWith("sha1.fp.")
    assertThat(key).isNotEqualTo("sha1.fp")
    // The scope reaches the hasher (3rd arg) so unchanged files skip content reads.
    verify(buildGraphHasher)
        .hashAllBazelTargetsAndSourcefiles(
            eq(emptySet()), eq(emptySet()), eq(modified), anyOrNull())
  }

  @Test
  fun scopedCacheKeyIsOrderIndependentAndDiffersFromBase() {
    val service = newService(RecordingGitClient(), InMemoryStorage())

    val base = service.cacheKey("sha1")
    val ab = service.cacheKey("sha1", setOf(Path.of("a"), Path.of("b")))
    val ba = service.cacheKey("sha1", setOf(Path.of("b"), Path.of("a")))

    assertThat(base).isEqualTo("sha1.fp")
    assertThat(ab).isEqualTo(ba) // digest is over the sorted set, so ordering can't matter
    assertThat(ab).isNotEqualTo(base)
  }

  @Test
  fun secondCallIsServedFromCacheWithoutRegenerating() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull()))
        .thenReturn(sampleHashes)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn(null) }
    val git = RecordingGitClient()
    val storage = InMemoryStorage()
    val service = newService(git, storage)

    service.getHashes("sha1")
    val second = service.getHashes("sha1")

    assertThat(second.hashes).isEqualTo(sampleHashes)
    // Only the first call touches the workspace / runs the hasher.
    assertThat(git.checkouts).isEqualTo(listOf("sha1"))
    verify(buildGraphHasher, times(1))
        .hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull())
  }

  @Test
  fun profilerRecordsCacheMissThenHit() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull()))
        .thenReturn(sampleHashes)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn(null) }
    val service = newService(RecordingGitClient(), InMemoryStorage())

    val missProfiler = QueryProfiler()
    service.getHashes("sha1", profiler = missProfiler)
    val hitProfiler = QueryProfiler()
    service.getHashes("sha1", profiler = hitProfiler)

    val miss = missProfiler.queryProfile().hashRetrievals.single()
    assertThat(miss.sha).isEqualTo("sha1")
    assertThat(miss.cacheHit).isEqualTo(false)
    assertThat(miss.durationMillis >= 0).isEqualTo(true)
    // A miss carries the per-phase generation breakdown (and no cache-read time).
    assertThat(miss.cacheReadMillis).isNull()
    val generation = miss.generation!!
    assertThat(generation.targetCount).isEqualTo(sampleHashes.size)
    assertThat(generation.checkoutMillis >= 0).isEqualTo(true)
    assertThat(miss.lockWaitMillis!! >= 0).isEqualTo(true)
    val hit = hitProfiler.queryProfile().hashRetrievals.single()
    assertThat(hit.sha).isEqualTo("sha1")
    assertThat(hit.cacheHit).isEqualTo(true)
    // A hit carries the read+deserialize time and no generation breakdown.
    assertThat(hit.cacheReadMillis!! >= 0).isEqualTo(true)
    assertThat(hit.generation).isNull()
  }

  @Test
  fun withWorkspaceAtChecksOutThenRunsBlock() {
    val git = RecordingGitClient()
    val service = newService(git, InMemoryStorage())

    val result = service.withWorkspaceAt("sha9") { "ran" }

    assertThat(result).isEqualTo("ran")
    assertThat(git.checkouts).isEqualTo(listOf("sha9"))
  }

  @Test
  fun cachePreservesModuleGraphJson() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull()))
        .thenReturn(sampleHashes)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn("""{"graph":1}""") }
    val storage = InMemoryStorage()

    // Generate once, then read back through a fresh service over the same storage (cache hit path).
    newService(RecordingGitClient(), storage).getHashes("sha1")
    val fromCache = newService(RecordingGitClient(), storage).getHashes("sha1")

    assertThat(fromCache.hashes).isEqualTo(sampleHashes)
    assertThat(fromCache.moduleGraphJson).isEqualTo("""{"graph":1}""")
  }

  @Test
  fun trackDepsPersistsAndRoundTripsDepEdges() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull()))
        .thenReturn(sampleHashesWithDeps)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn(null) }
    val storage = InMemoryStorage()

    val generated = newService(RecordingGitClient(), storage, trackDeps = true).getHashes("sha1")
    // Cache JSON carries the dependency-edge adjacency list under metadata.depEdges.
    assertThat(String(storage.entries.values.single())).contains("depEdges")
    // Read back through a fresh service over the same storage (cache-hit path).
    val fromCache = newService(RecordingGitClient(), storage, trackDeps = true).getHashes("sha1")

    val expectedDepEdges = mapOf("//a:lib" to listOf("//b:lib"), "//b:lib" to emptyList())
    assertThat(generated.depEdges).isEqualTo(expectedDepEdges)
    assertThat(fromCache.depEdges).isEqualTo(expectedDepEdges)
  }

  @Test
  fun withoutTrackDepsCacheHasNoDepEdges() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any(), anyOrNull()))
        .thenReturn(sampleHashesWithDeps)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn(null) }
    val storage = InMemoryStorage()

    val generated = newService(RecordingGitClient(), storage, trackDeps = false).getHashes("sha1")

    assertThat(generated.depEdges).isEqualTo(emptyMap())
    assertThat(String(storage.entries.values.single())).doesNotContain("depEdges")
  }
}
