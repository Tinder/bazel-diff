package com.bazel_diff.server

import com.bazel_diff.log.Logger
import java.time.Duration
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/**
 * Periodically evicts entries from a [PrunableHashCacheStorage] to keep a long-running server's
 * cache within [limits], preventing the per-commit-SHA cache from growing without bound.
 *
 * Sweeping runs on a single daemon thread so a clean JVM shutdown is never blocked, and uses a
 * fixed *delay* (not rate): the next sweep starts [interval] after the previous one finishes, so a
 * slow sweep over a large cache directory can never let runs pile up. The first sweep runs
 * immediately on [start] so a freshly (re)started server reclaims disk at once rather than after a
 * full interval.
 *
 * A no-op when [limits] sets no bounds ([CachePruneLimits.hasAny] is false).
 */
class CachePruner(
    private val storage: PrunableHashCacheStorage,
    private val limits: CachePruneLimits,
    private val interval: Duration,
) : KoinComponent {
  private val logger: Logger by inject()

  private val executor: ScheduledExecutorService =
      Executors.newSingleThreadScheduledExecutor { runnable ->
        Thread(runnable, "bazel-diff-cache-pruner").apply { isDaemon = true }
      }

  /**
   * Schedules the recurring sweep (and an immediate first pass). Idempotent limits-wise: a no-op
   * when no bound is configured.
   */
  fun start() {
    if (!limits.hasAny) {
      logger.i { "cache pruning disabled: no --cacheMax* limit set" }
      return
    }
    val periodSeconds = interval.seconds.coerceAtLeast(1)
    executor.scheduleWithFixedDelay(::runOnce, 0, periodSeconds, TimeUnit.SECONDS)
    logger.i { "cache pruner active (limits=$limits, sweeping every ${periodSeconds}s)" }
  }

  /** Stops the background sweeper. Safe to call more than once. */
  fun stop() {
    executor.shutdownNow()
  }

  /**
   * Runs one prune pass. Public so tests can drive a sweep deterministically. Never throws: a
   * transient filesystem error is logged and the next scheduled sweep retries, so a single failure
   * cannot kill the sweeper thread (which would silently disable pruning for the process lifetime).
   */
  fun runOnce() {
    try {
      val result = storage.prune(limits)
      if (result.evicted > 0) {
        logger.i {
          "cache prune evicted ${result.evicted} of ${result.scanned} entries " +
              "(${result.bytesFreed} bytes freed)"
        }
      }
    } catch (e: Exception) {
      logger.e(e) { "cache prune pass failed; retrying next interval" }
    }
  }
}
