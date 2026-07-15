package com.bazel_diff.server

import java.io.IOException
import java.nio.file.Files
import java.nio.file.NoSuchFileException
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.nio.file.attribute.FileTime
import java.time.Duration

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

/** Footprint of a cache: how many entries are stored and their total size in bytes. */
data class CacheStorageStats(val entryCount: Long, val totalBytes: Long)

/**
 * A [HashCacheStorage] that can cheaply report its footprint, surfaced by the `/metrics` endpoint
 * so callers can watch cache growth. Backends where size is not cheaply knowable in-process (e.g. a
 * remote store) simply do not implement this, and metrics report the size as unavailable.
 */
interface MeasurableHashCacheStorage : HashCacheStorage {
  fun stats(): CacheStorageStats
}

/**
 * Bounds a [PrunableHashCacheStorage] may be pruned down to. Each dimension is independent; a null
 * field means "no limit on that dimension", and a value of all-null disables pruning entirely (see
 * [hasAny]). Combining limits is allowed -- e.g. `maxAge` to expire stale revisions plus `maxBytes`
 * as a hard ceiling on disk use.
 *
 * @param maxAge evict entries not used (read or written) within this window.
 * @param maxEntries keep at most this many entries, evicting the least-recently-used first.
 * @param maxBytes keep the total stored size at or below this many bytes, evicting the
 *   least-recently-used first.
 */
data class CachePruneLimits(
    val maxAge: Duration? = null,
    val maxEntries: Int? = null,
    val maxBytes: Long? = null,
) {
  /** True when at least one dimension is limited; pruning is a no-op otherwise. */
  val hasAny: Boolean
    get() = maxAge != null || maxEntries != null || maxBytes != null
}

/** What a single [PrunableHashCacheStorage.prune] pass did, for logging. */
data class CachePruneResult(val scanned: Int, val evicted: Int, val bytesFreed: Long)

/**
 * A [HashCacheStorage] that can evict its own entries to stay within [CachePruneLimits], bounding
 * the footprint of a long-running server. Backends whose retention is managed externally (e.g. an
 * S3 bucket lifecycle policy) simply do not implement this interface, and the caller
 * ([CachePruner]) leaves them alone.
 */
interface PrunableHashCacheStorage : HashCacheStorage {
  /**
   * Evicts entries violating [limits] and returns what was removed. Never throws for a single
   * unreadable/undeletable entry -- pruning is best-effort.
   */
  fun prune(limits: CachePruneLimits): CachePruneResult
}

/**
 * [HashCacheStorage] that stores each entry as a file `<key>.json` under [directory]. The directory
 * is created if it does not exist.
 *
 * Recency is tracked via each file's last-modified time: [put] sets it (atomic move), and [get]
 * bumps it on a cache hit, so [prune] can evict by genuine least-recent *use* (LRU) rather than by
 * age since generation -- a base revision queried every CI run stays warm and is not expired out
 * from under active traffic.
 */
class LocalDiskHashCacheStorage(private val directory: Path) :
    MeasurableHashCacheStorage, PrunableHashCacheStorage {
  init {
    Files.createDirectories(directory)
  }

  private fun pathFor(key: String): Path = directory.resolve("$key.json")

  override fun get(key: String): ByteArray? {
    val path = pathFor(key)
    if (!Files.isRegularFile(path)) return null
    return try {
      val data = Files.readAllBytes(path)
      // Mark the entry as freshly used so the LRU-based pruner keeps hot revisions.
      touchQuietly(path)
      data
    } catch (e: NoSuchFileException) {
      // Pruned (or otherwise removed) between the existence check and the read: treat as a miss.
      null
    }
  }

  override fun put(key: String, data: ByteArray) {
    // Write to a temp file in the same directory then atomically move it into place, so a crash
    // mid-write never leaves a truncated entry that a later read would treat as a valid cache hit.
    // The `.tmp` suffix also keeps in-progress writes out of prune()'s `*.json` scan.
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

  override fun stats(): CacheStorageStats {
    if (!Files.isDirectory(directory)) return CacheStorageStats(0, 0)
    var entryCount = 0L
    var totalBytes = 0L
    // Only the `<key>.json` cache entries count -- in-progress `.tmp` writes are excluded, matching
    // what a cache read would ever see.
    Files.newDirectoryStream(directory, "*.json").use { stream ->
      for (path in stream) {
        try {
          if (Files.isRegularFile(path)) {
            entryCount++
            totalBytes += Files.size(path)
          }
        } catch (e: IOException) {
          // An entry that vanished mid-scan just doesn't count; keep tallying the rest.
        }
      }
    }
    return CacheStorageStats(entryCount, totalBytes)
  }

  override fun prune(limits: CachePruneLimits): CachePruneResult {
    if (!limits.hasAny) return CachePruneResult(0, 0, 0)

    // Snapshot the entries oldest-first. Files that vanish mid-scan (a concurrent prune elsewhere,
    // or an OS cleanup) are skipped rather than failing the whole pass.
    val entries = listEntries().sortedBy { it.lastUsedMillis }
    val evict = LinkedHashSet<Entry>()

    // Age: expire anything older than the cutoff regardless of the count/size budgets.
    limits.maxAge?.let { age ->
      val cutoff = System.currentTimeMillis() - age.toMillis()
      entries.forEach { if (it.lastUsedMillis < cutoff) evict += it }
    }

    // Survivors (still oldest-first) feed the count and size caps, which drop from the oldest end.
    val survivors = entries.filterNot { it in evict }.toMutableList()

    limits.maxEntries?.let { max ->
      while (survivors.size > max.coerceAtLeast(0)) evict += survivors.removeAt(0)
    }

    limits.maxBytes?.let { max ->
      var total = survivors.sumOf { it.size }
      while (total > max.coerceAtLeast(0) && survivors.isNotEmpty()) {
        val oldest = survivors.removeAt(0)
        evict += oldest
        total -= oldest.size
      }
    }

    var evicted = 0
    var bytesFreed = 0L
    for (entry in evict) {
      try {
        if (Files.deleteIfExists(entry.path)) {
          evicted++
          bytesFreed += entry.size
        }
      } catch (e: IOException) {
        // Best-effort: an entry we cannot delete (e.g. a transient lock) is retried next sweep.
      }
    }
    return CachePruneResult(entries.size, evicted, bytesFreed)
  }

  /** A cache file plus the metadata prune() sorts and budgets on. */
  private class Entry(val path: Path, val size: Long, val lastUsedMillis: Long)

  private fun listEntries(): List<Entry> {
    if (!Files.isDirectory(directory)) return emptyList()
    return Files.newDirectoryStream(directory, "*.json").use { stream ->
      stream.mapNotNull { path ->
        try {
          if (Files.isRegularFile(path)) {
            Entry(path, Files.size(path), Files.getLastModifiedTime(path).toMillis())
          } else null
        } catch (e: IOException) {
          null // vanished or unreadable mid-scan
        }
      }
    }
  }

  private fun touchQuietly(path: Path) {
    try {
      Files.setLastModifiedTime(path, FileTime.fromMillis(System.currentTimeMillis()))
    } catch (e: IOException) {
      // The mtime bump is only LRU bookkeeping; failing it must never fail the read.
    }
  }
}
