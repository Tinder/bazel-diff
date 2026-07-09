package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isNull
import assertk.assertions.isTrue
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class LocalDiskHashCacheStorageTest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private fun storage() = LocalDiskHashCacheStorage(temp.root.toPath())

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
}
