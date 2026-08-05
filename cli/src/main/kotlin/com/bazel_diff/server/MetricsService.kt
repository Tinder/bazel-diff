package com.bazel_diff.server

import java.util.Locale

/**
 * A point-in-time snapshot of a running server instance, returned by `GET /metrics` so operators
 * and callers can see the instance's identity, liveness, and resource usage without scraping logs.
 */
data class ServerMetrics(
    val version: String,
    val uptimeSeconds: Long,
    val ready: Boolean,
    val gitEngine: String,
    val trackDeps: Boolean,
    val cache: CacheMetrics,
    val jvm: JvmMetrics,
)

/**
 * Footprint of the hash cache. [entries]/[sizeBytes]/[sizeHuman] are null when the backend does not
 * cheaply report its size (e.g. a remote store) -- see [MeasurableHashCacheStorage]. [remote] is
 * the shared remote tier's location (e.g. `s3://bucket/prefix/`) when one is configured, null for a
 * local-only cache; the size fields always describe the local tier only.
 */
data class CacheMetrics(
    val directory: String?,
    val remote: String?,
    val entries: Long?,
    val sizeBytes: Long?,
    val sizeHuman: String?,
)

/** JVM heap usage of the instance, in bytes. */
data class JvmMetrics(val usedBytes: Long, val maxBytes: Long)

/**
 * Supplies a fresh [ServerMetrics] snapshot. Behind an interface so the HTTP layer can be tested
 * with a fake.
 */
interface MetricsProvider {
  fun snapshot(): ServerMetrics
}

/**
 * [MetricsProvider] that assembles a snapshot from the instance's configuration, its readiness
 * flag, live JVM state, and -- when the backing [storage] supports it
 * ([MeasurableHashCacheStorage]) -- the on-disk cache footprint. The cache size is read on demand
 * per request; `/metrics` is expected to be polled by monitoring at a low rate, so walking the
 * cache directory each call is acceptable.
 *
 * @param clock source of "now" in epoch millis, injectable so uptime is deterministic under test.
 * @param readiness the same liveness flag `/health` reports; surfaced here so a scrape of an
 *   un-ready or lame-ducked instance still returns data (metrics are intentionally not gated on
 *   it).
 */
class MetricsService(
    private val version: String,
    private val startedAtMillis: Long,
    private val gitEngine: String,
    private val trackDeps: Boolean,
    private val cacheDir: String,
    private val storage: HashCacheStorage,
    private val remoteCache: String? = null,
    private val readiness: () -> Boolean,
    private val clock: () -> Long = System::currentTimeMillis,
) : MetricsProvider {
  override fun snapshot(): ServerMetrics {
    val cacheStats = (storage as? MeasurableHashCacheStorage)?.stats()
    val runtime = Runtime.getRuntime()
    return ServerMetrics(
        version = version,
        uptimeSeconds = (clock() - startedAtMillis).coerceAtLeast(0) / 1000,
        ready = readiness(),
        gitEngine = gitEngine,
        trackDeps = trackDeps,
        cache =
            CacheMetrics(
                directory = cacheDir,
                remote = remoteCache,
                entries = cacheStats?.entryCount,
                sizeBytes = cacheStats?.totalBytes,
                sizeHuman = cacheStats?.totalBytes?.let(::humanReadableBytes),
            ),
        jvm =
            JvmMetrics(
                usedBytes = runtime.totalMemory() - runtime.freeMemory(),
                maxBytes = runtime.maxMemory(),
            ),
    )
  }

  /**
   * Formats a byte count as a compact base-1024 string (matching the `--cacheMaxSize` unit scale),
   * e.g. `4.6 MB`.
   */
  private fun humanReadableBytes(bytes: Long): String {
    if (bytes < 1024) return "$bytes B"
    val units = listOf("KB", "MB", "GB", "TB")
    var value = bytes.toDouble() / 1024
    var unit = 0
    while (value >= 1024 && unit < units.size - 1) {
      value /= 1024
      unit++
    }
    return String.format(Locale.US, "%.1f %s", value, units[unit])
  }
}
