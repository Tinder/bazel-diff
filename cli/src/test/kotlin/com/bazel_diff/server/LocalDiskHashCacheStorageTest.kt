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
  fun statsCountsEntriesAndTotalBytes() {
    val storage = storage()
    storage.put("a", ByteArray(100))
    storage.put("b", ByteArray(50))

    val stats = storage.stats()

    assertThat(stats.entryCount).isEqualTo(2L)
    assertThat(stats.totalBytes).isEqualTo(150L)
  }

  @Test
  fun statsIsEmptyWithNoEntriesAndIgnoresNonJsonFiles() {
    val dir = temp.root.toPath()
    val storage = LocalDiskHashCacheStorage(dir)
    assertThat(storage.stats()).isEqualTo(CacheStorageStats(0, 0))

    Files.write(dir.resolve("notes.txt"), "hello".toByteArray(StandardCharsets.UTF_8))
    storage.put("k", ByteArray(10))

    val stats = storage.stats()
    assertThat(stats.entryCount).isEqualTo(1L) // the .txt file is not counted
    assertThat(stats.totalBytes).isEqualTo(10L)
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

  @Test
  fun defaultContainsDelegatesToGet() {
    // LocalDiskHashCacheStorage overrides contains; exercise the interface default on a bare impl.
    val backing = mutableMapOf<String, ByteArray>()
    val storage =
        object : HashCacheStorage {
          override fun get(key: String): ByteArray? = backing[key]
          override fun put(key: String, data: ByteArray) {
            backing[key] = data
          }
        }
    assertThat(storage.contains("k")).isFalse()
    storage.put("k", bytes("v"))
    assertThat(storage.contains("k")).isTrue()
  }

  @Test
  fun getReturnsNullAfterEntryFileDeleted() {
    val storage = storage()
    storage.put("gone", bytes("x"))
    Files.delete(temp.root.toPath().resolve("gone.json"))
    assertThat(storage.get("gone")).isNull()
    assertThat(storage.contains("gone")).isFalse()
  }

  @Test
  fun statsAndPruneAreEmptyWhenDirectoryRemoved() {
    val dir = temp.newFolder("cache-dir").toPath()
    val storage = LocalDiskHashCacheStorage(dir)
    storage.put("k", bytes("v"))
    // Wipe the directory after construction: stats/prune must degrade gracefully.
    dir.toFile().deleteRecursively()

    assertThat(storage.stats()).isEqualTo(CacheStorageStats(0, 0))
    val result = storage.prune(CachePruneLimits(maxEntries = 1))
    assertThat(result).isEqualTo(CachePruneResult(0, 0, 0))
  }

  @Test
  fun pruneWithMaxEntriesZeroEvictsEverything() {
    val storage = storage()
    storage.put("a", bytes("a"))
    storage.put("b", bytes("b"))

    val result = storage.prune(CachePruneLimits(maxEntries = 0))

    assertThat(result.evicted).isEqualTo(2)
    assertThat(storage.contains("a")).isFalse()
    assertThat(storage.contains("b")).isFalse()
  }

  @Test
  fun pruneContinuesWhenAnEntryCannotBeDeleted() {
    // Best-effort prune: an undeletable entry must not abort the pass. Make the cache directory
    // non-writable so Deletes fail with AccessDeniedException (works on Linux and macOS; unlike
    // macOS-only `chflags uchg`, which is missing on Ubuntu CI).
    val dir = temp.newFolder("prune-locked").toPath()
    val storage = LocalDiskHashCacheStorage(dir)
    storage.put("a", bytes("a"))
    storage.put("b", bytes("b"))
    val originalPerms = Files.getPosixFilePermissions(dir)
    Files.setPosixFilePermissions(
        dir, java.nio.file.attribute.PosixFilePermissions.fromString("r-xr-xr-x"))
    try {
      val result = storage.prune(CachePruneLimits(maxEntries = 0))
      assertThat(result.scanned).isEqualTo(2)
      // Neither entry could be removed; prune still returns without throwing.
      assertThat(result.evicted).isEqualTo(0)
      assertThat(Files.isRegularFile(dir.resolve("a.json"))).isTrue()
      assertThat(Files.isRegularFile(dir.resolve("b.json"))).isTrue()
    } finally {
      Files.setPosixFilePermissions(dir, originalPerms)
    }
  }

  @Test
  fun getStillReturnsDataWhenTouchFails() {
    // touchQuietly must swallow IOException so a failed mtime bump never fails the read. Call it
    // directly with a missing path (setLastModifiedTime throws) — same catch as an immutable file.
    val storage = storage()
    storage.put("ok", bytes("payload"))
    storage.touchQuietlyForTest(temp.root.toPath().resolve("does-not-exist.json"))
    assertThat(String(storage.get("ok")!!, StandardCharsets.UTF_8)).isEqualTo("payload")
  }

  @Test
  fun interfaceTypedCallsReachStatsAndPrune() {
    val measurable: MeasurableHashCacheStorage = storage()
    assertThat(measurable.stats()).isEqualTo(CacheStorageStats(0, 0))
    val prunable: PrunableHashCacheStorage = storage()
    assertThat(prunable.prune(CachePruneLimits(maxBytes = 0))).isEqualTo(CachePruneResult(0, 0, 0))
  }
}
