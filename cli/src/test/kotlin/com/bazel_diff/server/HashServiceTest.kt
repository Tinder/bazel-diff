package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.hasSize
import assertk.assertions.isEqualTo
import assertk.assertions.isNull
import com.bazel_diff.SilentLogger
import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.log.Logger
import com.google.gson.GsonBuilder
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

  private fun newService(git: GitClient, storage: HashCacheStorage) =
      HashService(git, storage, "fp", emptySet(), emptySet())

  @Test
  fun cacheMissGeneratesAndStores() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any()))
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
  fun secondCallIsServedFromCacheWithoutRegenerating() {
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any()))
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
    verify(buildGraphHasher, times(1)).hashAllBazelTargetsAndSourcefiles(any(), any(), any())
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
    whenever(buildGraphHasher.hashAllBazelTargetsAndSourcefiles(any(), any(), any()))
        .thenReturn(sampleHashes)
    runBlocking { whenever(bazelModService.getModuleGraphJson()).thenReturn("""{"graph":1}""") }
    val storage = InMemoryStorage()

    // Generate once, then read back through a fresh service over the same storage (cache hit path).
    newService(RecordingGitClient(), storage).getHashes("sha1")
    val fromCache = newService(RecordingGitClient(), storage).getHashes("sha1")

    assertThat(fromCache.hashes).isEqualTo(sampleHashes)
    assertThat(fromCache.moduleGraphJson).isEqualTo("""{"graph":1}""")
  }
}
