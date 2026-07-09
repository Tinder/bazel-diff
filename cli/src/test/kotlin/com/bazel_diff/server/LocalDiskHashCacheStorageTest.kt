package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isNotNull
import assertk.assertions.isNull
import assertk.assertions.isTrue
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.attribute.FileTime
import java.time.Duration
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class LocalDiskHashCacheStorageTest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private fun storage() = LocalDiskHashCacheStorage(temp.root.toPath())

  private fun bytes(s: String) = s.toByteArray(StandardCharsets.UTF_8)

  /** Backdates an entry's last-used time so age/LRU ordering is deterministic under test. */
  private fun setAgeMinutes(key: String, minutes: Long) {
    val path = temp.root.toPath().resolve("$key.json")
    Files.setLastModifiedTime(
        path, FileTime.fromMillis(System.currentTimeMillis() - minutes * 60_000))
  }

  @Test
  fun missReturnsNull() {
    val storage = storage()
    assertThat(storage.get("absent")).isNull()
    assertThat(storage.contains("absent")).isFalse()
  }

  @Test
  fun putThenGetRoundTrips() {
    val storage = storage()
    storage.put("sha.fp", "hello".toByteArray(StandardCharsets.UTF_8))
    assertThat(storage.contains("sha.fp")).isTrue()
    assertThat(String(storage.get("sha.fp")!!, StandardCharsets.UTF_8)).isEqualTo("hello")
  }

  @Test
  fun putOverwrites() {
    val storage = storage()
    storage.put("k", "a".toByteArray(StandardCharsets.UTF_8))
    storage.put("k", "b".toByteArray(StandardCharsets.UTF_8))
    assertThat(String(storage.get("k")!!, StandardCharsets.UTF_8)).isEqualTo("b")
  }

  @Test
  fun createsDirectoryIfMissing() {
    val nested = temp.root.toPath().resolve("a/b/c")
    LocalDiskHashCacheStorage(nested)
    assertThat(Files.isDirectory(nested)).isTrue()
  }

  @Test
  fun persistsAcrossInstances() {
    val dir = temp.root.toPath()
    LocalDiskHashCacheStorage(dir).put("k", "v".toByteArray(StandardCharsets.UTF_8))
    // A fresh instance over the same directory must see the previously-written entry: the cache
    // has to survive process restarts (RFC issue #29).
    assertThat(String(LocalDiskHashCacheStorage(dir).get("k")!!, StandardCharsets.UTF_8))
        .isEqualTo("v")
  }

  @Test
  fun pruneWithNoLimitsRemovesNothing() {
    val storage = storage()
    storage.put("a", bytes("a"))
    val result = storage.prune(CachePruneLimits())
    assertThat(result.evicted).isEqualTo(0)
    assertThat(storage.contains("a")).isTrue()
  }

  @Test
  fun pruneByMaxAgeEvictsEntriesNotUsedWithinTheWindow() {
    val storage = storage()
    storage.put("old", bytes("x"))
    storage.put("fresh", bytes("y"))
    setAgeMinutes("old", 120)
    setAgeMinutes("fresh", 5)

    val result = storage.prune(CachePruneLimits(maxAge = Duration.ofHours(1)))

    assertThat(result.scanned).isEqualTo(2)
    assertThat(result.evicted).isEqualTo(1)
    assertThat(storage.contains("old")).isFalse()
    assertThat(storage.contains("fresh")).isTrue()
  }

  @Test
  fun pruneByMaxEntriesKeepsTheMostRecentlyUsed() {
    val storage = storage()
    storage.put("a", bytes("a"))
    storage.put("b", bytes("b"))
    storage.put("c", bytes("c"))
    setAgeMinutes("a", 30) // oldest
    setAgeMinutes("b", 20)
    setAgeMinutes("c", 10) // newest

    val result = storage.prune(CachePruneLimits(maxEntries = 2))

    assertThat(result.evicted).isEqualTo(1)
    assertThat(storage.contains("a")).isFalse()
    assertThat(storage.contains("b")).isTrue()
    assertThat(storage.contains("c")).isTrue()
  }

  @Test
  fun pruneByMaxSizeEvictsOldestUntilUnderBudget() {
    val storage = storage()
    // Each entry is exactly 100 bytes; a 250-byte budget keeps the two newest.
    storage.put("a", ByteArray(100))
    storage.put("b", ByteArray(100))
    storage.put("c", ByteArray(100))
    setAgeMinutes("a", 30)
    setAgeMinutes("b", 20)
    setAgeMinutes("c", 10)

    val result = storage.prune(CachePruneLimits(maxBytes = 250))

    assertThat(result.evicted).isEqualTo(1)
    assertThat(result.bytesFreed).isEqualTo(100)
    assertThat(storage.contains("a")).isFalse()
    assertThat(storage.contains("b")).isTrue()
    assertThat(storage.contains("c")).isTrue()
  }

  @Test
  fun getBumpsRecencySoActivelyUsedEntriesSurvivePruning() {
    val storage = storage()
    storage.put("a", bytes("a"))
    storage.put("b", bytes("b"))
    setAgeMinutes("a", 30)
    setAgeMinutes("b", 20)

    // A cache hit on the older entry makes it the most-recently-used, so the LRU count cap now
    // evicts "b" instead of "a".
    assertThat(storage.get("a")).isNotNull()
    storage.prune(CachePruneLimits(maxEntries = 1))

    assertThat(storage.contains("a")).isTrue()
    assertThat(storage.contains("b")).isFalse()
  }

  @Test
  fun pruneOnlyConsidersJsonEntriesAndLeavesOtherFilesAlone() {
    val storage = storage()
    storage.put("stale", bytes("x"))
    setAgeMinutes("stale", 120)
    val sibling = temp.root.toPath().resolve("README.txt")
    Files.write(sibling, bytes("not a cache entry"))

    val result = storage.prune(CachePruneLimits(maxAge = Duration.ofHours(1)))

    assertThat(result.scanned).isEqualTo(1) // the non-.json file is not counted
    assertThat(result.evicted).isEqualTo(1)
    assertThat(Files.exists(sibling)).isTrue()
  }
}
