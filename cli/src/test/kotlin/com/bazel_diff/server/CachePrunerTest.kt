package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isTrue
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import java.nio.file.Files
import java.time.Duration
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule

class CachePrunerTest : KoinTest {
  @get:Rule
  val koinTestRule = KoinTestRule.create { modules(module { single<Logger> { SilentLogger } }) }

  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  /** Records prune calls; optionally throws to exercise the sweeper's error handling. */
  private class FakeStorage(
      private val result: CachePruneResult = CachePruneResult(0, 0, 0),
      private val throwOnPrune: Boolean = false,
  ) : PrunableHashCacheStorage {
    val pruneCount = AtomicInteger(0)
    val firstPrune = CountDownLatch(1)
    @Volatile var lastLimits: CachePruneLimits? = null

    override fun get(key: String): ByteArray? = null

    override fun put(key: String, data: ByteArray) = Unit

    override fun prune(limits: CachePruneLimits): CachePruneResult {
      lastLimits = limits
      pruneCount.incrementAndGet()
      firstPrune.countDown()
      if (throwOnPrune) throw RuntimeException("boom")
      return result
    }
  }

  @Test
  fun startWithNoLimitsNeverSchedulesASweep() {
    val storage = FakeStorage()
    val pruner = CachePruner(storage, CachePruneLimits(), Duration.ofSeconds(1))
    // With no bound configured, start() returns before scheduling anything, so this is
    // deterministic.
    pruner.start()
    pruner.stop()
    assertThat(storage.pruneCount.get()).isEqualTo(0)
  }

  @Test
  fun startSchedulesAnImmediateSweepWithTheConfiguredLimits() {
    val storage = FakeStorage()
    val limits = CachePruneLimits(maxEntries = 5)
    val pruner = CachePruner(storage, limits, Duration.ofHours(1))
    try {
      pruner.start()
      assertThat(storage.firstPrune.await(5, TimeUnit.SECONDS)).isTrue()
      assertThat(storage.lastLimits).isEqualTo(limits)
    } finally {
      pruner.stop()
    }
  }

  @Test
  fun runOnceSwallowsStorageErrorsSoTheSweeperThreadSurvives() {
    val storage = FakeStorage(throwOnPrune = true)
    val pruner = CachePruner(storage, CachePruneLimits(maxEntries = 1), Duration.ofHours(1))
    // Driving a pass directly must not propagate the storage failure.
    pruner.runOnce()
    assertThat(storage.pruneCount.get()).isEqualTo(1)
    pruner.stop()
  }

  @Test
  fun endToEndTheScheduledSweeperEvictsRealFilesDownToTheLimit() {
    // Wire the real scheduler to the real on-disk storage: writing more entries than the cap and
    // starting the pruner must actually delete files from the cache directory.
    val dir = temp.newFolder().toPath()
    val storage = LocalDiskHashCacheStorage(dir)
    repeat(5) { i -> storage.put("k$i", ByteArray(10)) }
    val pruner = CachePruner(storage, CachePruneLimits(maxEntries = 2), Duration.ofSeconds(1))
    try {
      pruner.start()
      val deadline = System.currentTimeMillis() + 5_000
      while (jsonCount(dir) > 2 && System.currentTimeMillis() < deadline) {
        Thread.sleep(25)
      }
      assertThat(jsonCount(dir)).isEqualTo(2)
    } finally {
      pruner.stop()
    }
  }

  private fun jsonCount(dir: java.nio.file.Path): Int =
      Files.newDirectoryStream(dir, "*.json").use { it.count() }
}
