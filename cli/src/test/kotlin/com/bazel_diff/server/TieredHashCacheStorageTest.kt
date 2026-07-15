package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isNull
import assertk.assertions.isTrue
import java.nio.charset.StandardCharsets
import org.junit.Test

class TieredHashCacheStorageTest {
  /** In-memory local tier that records prune calls and reports canned stats. */
  private class FakeLocal : MeasurableHashCacheStorage, PrunableHashCacheStorage {
    val entries = mutableMapOf<String, ByteArray>()
    var gets = 0
    var prunedWith: CachePruneLimits? = null

    override fun get(key: String): ByteArray? {
      gets++
      return entries[key]
    }

    override fun put(key: String, data: ByteArray) {
      entries[key] = data
    }

    override fun stats() = CacheStorageStats(entryCount = entries.size.toLong(), totalBytes = 42)

    override fun prune(limits: CachePruneLimits): CachePruneResult {
      prunedWith = limits
      return CachePruneResult(scanned = entries.size, evicted = 1, bytesFreed = 10)
    }
  }

  /** In-memory remote tier that counts reads and writes. */
  private class FakeRemote : HashCacheStorage {
    val entries = mutableMapOf<String, ByteArray>()
    var gets = 0
    var puts = 0

    override fun get(key: String): ByteArray? {
      gets++
      return entries[key]
    }

    override fun put(key: String, data: ByteArray) {
      puts++
      entries[key] = data
    }
  }

  private val local = FakeLocal()
  private val remote = FakeRemote()
  private val tiered = TieredHashCacheStorage(local, remote)

  private fun bytes(s: String) = s.toByteArray(StandardCharsets.UTF_8)

  @Test
  fun localHitNeverTouchesRemote() {
    local.entries["k"] = bytes("local")

    assertThat(String(tiered.get("k")!!, StandardCharsets.UTF_8)).isEqualTo("local")
    assertThat(remote.gets).isEqualTo(0)
  }

  @Test
  fun remoteHitIsBackfilledIntoLocal() {
    remote.entries["k"] = bytes("shared")

    assertThat(String(tiered.get("k")!!, StandardCharsets.UTF_8)).isEqualTo("shared")
    // The next read must be served locally: the whole point of the backfill.
    assertThat(String(local.entries["k"]!!, StandardCharsets.UTF_8)).isEqualTo("shared")
    assertThat(String(tiered.get("k")!!, StandardCharsets.UTF_8)).isEqualTo("shared")
    assertThat(remote.gets).isEqualTo(1)
  }

  @Test
  fun missInBothTiersReturnsNull() {
    assertThat(tiered.get("absent")).isNull()
    assertThat(remote.gets).isEqualTo(1)
  }

  @Test
  fun putWritesBothTiers() {
    tiered.put("k", bytes("v"))

    assertThat(String(local.entries["k"]!!, StandardCharsets.UTF_8)).isEqualTo("v")
    assertThat(String(remote.entries["k"]!!, StandardCharsets.UTF_8)).isEqualTo("v")
    assertThat(remote.puts).isEqualTo(1)
  }

  @Test
  fun containsChecksBothTiers() {
    assertThat(tiered.contains("absent")).isFalse()

    local.entries["l"] = bytes("x")
    assertThat(tiered.contains("l")).isTrue()

    remote.entries["r"] = bytes("y")
    assertThat(tiered.contains("r")).isTrue()
  }

  @Test
  fun statsReportTheLocalTierOnly() {
    local.entries["a"] = bytes("x")
    remote.entries["b"] = bytes("y")

    // Remote footprint is the bucket's business (lifecycle policy), not /metrics'.
    assertThat(tiered.stats()).isEqualTo(CacheStorageStats(entryCount = 1, totalBytes = 42))
  }

  @Test
  fun pruneBoundsTheLocalTierOnly() {
    remote.entries["b"] = bytes("y")
    val limits = CachePruneLimits(maxEntries = 5)

    val result = tiered.prune(limits)

    assertThat(local.prunedWith).isEqualTo(limits)
    assertThat(result.evicted).isEqualTo(1)
    // The remote entry is untouched by local pruning.
    assertThat(remote.entries.containsKey("b")).isTrue()
  }
}
