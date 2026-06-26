package com.bazel_diff.server

import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption

/**
 * Persistent store for generated hash JSON, keyed by an opaque cache key (a git SHA combined with a
 * config fingerprint -- see [HashService.cacheKey]).
 *
 * Implementations should persist across process restarts: regenerating hashes on a cache miss is
 * expensive (a full `bazel query` over the workspace), which is the whole reason the query service
 * caches. The interface is intentionally byte-oriented so an S3-backed implementation can drop in
 * behind it without touching callers -- the RFC (issue #29) calls out S3 as the expected backend.
 */
interface HashCacheStorage {
  /** Returns the stored bytes for [key], or null on a cache miss. */
  fun get(key: String): ByteArray?

  /** Stores [data] under [key], overwriting any previous entry. */
  fun put(key: String, data: ByteArray)

  /** True if [key] has a stored entry. */
  fun contains(key: String): Boolean = get(key) != null
}

/**
 * [HashCacheStorage] that stores each entry as a file `<key>.json` under [directory]. The directory
 * is created if it does not exist.
 */
class LocalDiskHashCacheStorage(private val directory: Path) : HashCacheStorage {
  init {
    Files.createDirectories(directory)
  }

  private fun pathFor(key: String): Path = directory.resolve("$key.json")

  override fun get(key: String): ByteArray? {
    val path = pathFor(key)
    return if (Files.isRegularFile(path)) Files.readAllBytes(path) else null
  }

  override fun put(key: String, data: ByteArray) {
    // Write to a temp file in the same directory then atomically move it into place, so a crash
    // mid-write never leaves a truncated entry that a later read would treat as a valid cache hit.
    val target = pathFor(key)
    val tmp = Files.createTempFile(directory, "$key-", ".tmp")
    try {
      Files.write(tmp, data)
      Files.move(tmp, target, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE)
    } finally {
      Files.deleteIfExists(tmp)
    }
  }

  override fun contains(key: String): Boolean = Files.isRegularFile(pathFor(key))
}
