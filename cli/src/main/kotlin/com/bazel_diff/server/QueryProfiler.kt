package com.bazel_diff.server

import java.lang.management.ManagementFactory
import java.util.Collections

/**
 * Timing of one hash retrieval (one side of a query). [cacheHit] is true when the hashes were
 * served from the per-SHA cache -- including when this request waited behind another request that
 * generated them -- and false when this request ran the checkout + `bazel query` itself, which is
 * why a miss is typically orders of magnitude slower than a hit.
 */
data class HashRetrievalProfile(
    val sha: String,
    val cacheHit: Boolean,
    val durationMillis: Long,
)

/**
 * Wall-clock breakdown of one impacted-targets query, attached to the response when the request
 * opts in with `profile=true` so callers can feed per-request latency and cache effectiveness into
 * their metrics and monitoring.
 *
 * [totalDurationMillis] spans from request dispatch to response assembly and includes any wait for
 * the workspace lock, so it can exceed the sum of the individual phases.
 * [resolveRevisionsDurationMillis] covers git revision resolution including any on-demand fetches.
 * [diffDurationMillis] covers the hash diff itself and, when the module graph changed between the
 * revisions, the live `rdeps` query that path issues.
 */
data class QueryProfile(
    val totalDurationMillis: Long,
    val resolveRevisionsDurationMillis: Long,
    val hashRetrievals: List<HashRetrievalProfile>,
    val diffDurationMillis: Long,
)

/**
 * JVM memory movement over one impacted-targets query, attached to the response when the request
 * opts in with `profile=true`. Heap and GC numbers are process-wide, so concurrent requests and
 * background work bleed into each other's deltas -- treat them as indicative, not exact.
 */
data class MemoryProfile(
    val heapUsedBeforeBytes: Long,
    val heapUsedAfterBytes: Long,
    val heapUsedDeltaBytes: Long,
    val heapMaxBytes: Long,
    val gcCollections: Long,
    val gcTimeMillis: Long,
)

/** Cumulative GC activity of the JVM at a point in time (summed across all collectors). */
data class GcStats(val collections: Long, val timeMillis: Long)

/** Reads the live cumulative GC stats; a collector that reports -1 (unavailable) counts as 0. */
private fun liveGcStats(): GcStats {
  var collections = 0L
  var timeMillis = 0L
  for (bean in ManagementFactory.getGarbageCollectorMXBeans()) {
    collections += bean.collectionCount.coerceAtLeast(0)
    timeMillis += bean.collectionTime.coerceAtLeast(0)
  }
  return GcStats(collections, timeMillis)
}

/**
 * Per-request collector for [QueryProfile]/[MemoryProfile]. Constructed by the HTTP layer when a
 * request asks for profiling (`profile=true`) and threaded through the services, which record each
 * phase as it completes; the memory/time baselines are captured at construction. All suppliers are
 * injectable so tests can drive deterministic values.
 *
 * Phase durations are recorded (not measured here) so the services stay in control of what a phase
 * spans; only [totalDurationMillis][QueryProfile.totalDurationMillis] uses [nanoClock] directly.
 * Recording is thread-safe, but a profiler instance belongs to a single request.
 */
class QueryProfiler(
    private val nanoClock: () -> Long = System::nanoTime,
    private val heapUsed: () -> Long = {
      Runtime.getRuntime().let { it.totalMemory() - it.freeMemory() }
    },
    private val heapMax: () -> Long = { Runtime.getRuntime().maxMemory() },
    private val gcStats: () -> GcStats = ::liveGcStats,
) {
  private val startNanos = nanoClock()
  private val heapUsedBefore = heapUsed()
  private val gcBefore = gcStats()

  @Volatile private var resolveRevisionsMillis = 0L
  @Volatile private var diffMillis = 0L
  private val hashRetrievals = Collections.synchronizedList(mutableListOf<HashRetrievalProfile>())

  fun recordResolveRevisions(durationMillis: Long) {
    resolveRevisionsMillis += durationMillis
  }

  fun recordHashRetrieval(sha: String, cacheHit: Boolean, durationMillis: Long) {
    hashRetrievals.add(HashRetrievalProfile(sha, cacheHit, durationMillis))
  }

  fun recordDiff(durationMillis: Long) {
    diffMillis += durationMillis
  }

  /** Snapshot of the recorded phases; total spans construction to this call. */
  fun queryProfile(): QueryProfile =
      QueryProfile(
          totalDurationMillis = (nanoClock() - startNanos) / 1_000_000,
          resolveRevisionsDurationMillis = resolveRevisionsMillis,
          hashRetrievals = synchronized(hashRetrievals) { hashRetrievals.toList() },
          diffDurationMillis = diffMillis,
      )

  /** Heap/GC movement between construction and this call. */
  fun memoryProfile(): MemoryProfile {
    val heapUsedAfter = heapUsed()
    val gcAfter = gcStats()
    return MemoryProfile(
        heapUsedBeforeBytes = heapUsedBefore,
        heapUsedAfterBytes = heapUsedAfter,
        heapUsedDeltaBytes = heapUsedAfter - heapUsedBefore,
        heapMaxBytes = heapMax(),
        gcCollections = (gcAfter.collections - gcBefore.collections).coerceAtLeast(0),
        gcTimeMillis = (gcAfter.timeMillis - gcBefore.timeMillis).coerceAtLeast(0),
    )
  }
}
