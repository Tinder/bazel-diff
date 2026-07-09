package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isNull
import assertk.assertions.isTrue
import org.junit.Test

class MetricsServiceTest {
  private class FakeMeasurable(private val stats: CacheStorageStats) : MeasurableHashCacheStorage {
    override fun get(key: String): ByteArray? = null

    override fun put(key: String, data: ByteArray) = Unit

    override fun stats(): CacheStorageStats = stats
  }

  private class FakePlain : HashCacheStorage {
    override fun get(key: String): ByteArray? = null

    override fun put(key: String, data: ByteArray) = Unit
  }

  private fun service(
      storage: HashCacheStorage,
      ready: Boolean = true,
      nowMillis: Long = 10_000,
      startedAtMillis: Long = 4_000,
  ) =
      MetricsService(
          version = "1.2.3",
          startedAtMillis = startedAtMillis,
          gitEngine = "jgit",
          trackDeps = true,
          cacheDir = "/var/cache/bazel-diff",
          storage = storage,
          readiness = { ready },
          clock = { nowMillis },
      )

  @Test
  fun reportsInstanceInfoAndCacheStats() {
    val snap =
        service(FakeMeasurable(CacheStorageStats(entryCount = 3, totalBytes = 3072))).snapshot()

    assertThat(snap.version).isEqualTo("1.2.3")
    assertThat(snap.uptimeSeconds).isEqualTo(6) // (10000 - 4000) / 1000
    assertThat(snap.ready).isTrue()
    assertThat(snap.gitEngine).isEqualTo("jgit")
    assertThat(snap.trackDeps).isTrue()
    assertThat(snap.cache.directory).isEqualTo("/var/cache/bazel-diff")
    assertThat(snap.cache.entries).isEqualTo(3L)
    assertThat(snap.cache.sizeBytes).isEqualTo(3072L)
    assertThat(snap.cache.sizeHuman).isEqualTo("3.0 KB")
    assertThat(snap.jvm.maxBytes > 0L).isTrue()
  }

  @Test
  fun cacheStatsAreNullWhenBackendIsNotMeasurable() {
    val snap = service(FakePlain()).snapshot()

    // The configured directory is still reported, but the size is unknown for a non-measurable
    // store.
    assertThat(snap.cache.directory).isEqualTo("/var/cache/bazel-diff")
    assertThat(snap.cache.entries).isNull()
    assertThat(snap.cache.sizeBytes).isNull()
    assertThat(snap.cache.sizeHuman).isNull()
  }

  @Test
  fun readinessIsReflectedLive() {
    assertThat(service(FakePlain(), ready = false).snapshot().ready).isFalse()
  }

  @Test
  fun uptimeIsClampedToZeroOnClockSkew() {
    // Started "after" the current clock reading -> report 0, never a negative uptime.
    val snap = service(FakePlain(), nowMillis = 1_000, startedAtMillis = 5_000).snapshot()
    assertThat(snap.uptimeSeconds).isEqualTo(0)
  }
}
