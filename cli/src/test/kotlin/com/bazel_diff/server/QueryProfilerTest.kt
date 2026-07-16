package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isGreaterThan
import assertk.assertions.isGreaterThanOrEqualTo
import org.junit.Test

class QueryProfilerTest {

  @Test
  fun computesPhaseTimingsAndTotalFromInjectedClock() {
    var nowNanos = 0L
    val profiler =
        QueryProfiler(
            nanoClock = { nowNanos },
            heapUsed = { 0 },
            heapMax = { 1 },
            gcStats = { GcStats(0, 0) })

    val generation =
        HashGenerationBreakdown(
            checkoutMillis = 20,
            bazelQueryMillis = 150,
            sourceHashMillis = 30,
            targetHashMillis = 25,
            moduleGraphMillis = 15,
            cacheWriteMillis = 5,
            targetCount = 1234)
    profiler.recordResolveRevisions(3)
    profiler.recordHashRetrieval(
        HashRetrievalProfile("from-sha", cacheHit = true, durationMillis = 10, cacheReadMillis = 9))
    profiler.recordHashRetrieval(
        HashRetrievalProfile(
            "to-sha",
            cacheHit = false,
            durationMillis = 250,
            lockWaitMillis = 2,
            generation = generation))
    profiler.recordDiff(7)
    nowNanos = 321_000_000 // 321ms since construction

    val profile = profiler.queryProfile()

    assertThat(profile.totalDurationMillis).isEqualTo(321)
    assertThat(profile.resolveRevisionsDurationMillis).isEqualTo(3)
    assertThat(profile.diffDurationMillis).isEqualTo(7)
    assertThat(profile.diffModuleGraphChanged).isEqualTo(false)
    assertThat(profile.hashRetrievals)
        .isEqualTo(
            listOf(
                HashRetrievalProfile("from-sha", true, 10, cacheReadMillis = 9),
                HashRetrievalProfile(
                    "to-sha", false, 250, lockWaitMillis = 2, generation = generation)))
  }

  @Test
  fun diffModuleGraphChangedLatchesAcrossRecordings() {
    val profiler =
        QueryProfiler(
            nanoClock = { 0 }, heapUsed = { 0 }, heapMax = { 1 }, gcStats = { GcStats(0, 0) })

    profiler.recordDiff(5, moduleGraphChanged = true)
    // A later recording without the flag must not clear it.
    profiler.recordDiff(2)

    val profile = profiler.queryProfile()
    assertThat(profile.diffDurationMillis).isEqualTo(7)
    assertThat(profile.diffModuleGraphChanged).isEqualTo(true)
  }

  @Test
  fun computesMemoryDeltasBetweenConstructionAndSnapshot() {
    var heap = 100L
    var gc = GcStats(collections = 5, timeMillis = 50)
    val profiler =
        QueryProfiler(nanoClock = { 0 }, heapUsed = { heap }, heapMax = { 4096 }, gcStats = { gc })

    heap = 350
    gc = GcStats(collections = 7, timeMillis = 90)
    val memory = profiler.memoryProfile()

    assertThat(memory.heapUsedBeforeBytes).isEqualTo(100)
    assertThat(memory.heapUsedAfterBytes).isEqualTo(350)
    assertThat(memory.heapUsedDeltaBytes).isEqualTo(250)
    assertThat(memory.heapMaxBytes).isEqualTo(4096)
    assertThat(memory.gcCollections).isEqualTo(2)
    assertThat(memory.gcTimeMillis).isEqualTo(40)
  }

  @Test
  fun heapDeltaCanBeNegativeButGcDeltasNever() {
    // Heap shrinking across the query (a GC ran) is meaningful and must be reported as-is; a GC
    // counter moving backwards is only ever collector-bean noise and is clamped instead.
    var heap = 500L
    var gc = GcStats(collections = 5, timeMillis = 50)
    val profiler =
        QueryProfiler(nanoClock = { 0 }, heapUsed = { heap }, heapMax = { 4096 }, gcStats = { gc })

    heap = 200
    gc = GcStats(collections = 4, timeMillis = 40)
    val memory = profiler.memoryProfile()

    assertThat(memory.heapUsedDeltaBytes).isEqualTo(-300)
    assertThat(memory.gcCollections).isEqualTo(0)
    assertThat(memory.gcTimeMillis).isEqualTo(0)
  }

  @Test
  fun liveDefaultsProduceSaneValues() {
    val profiler = QueryProfiler()

    val profile = profiler.queryProfile()
    val memory = profiler.memoryProfile()

    assertThat(profile.totalDurationMillis).isGreaterThanOrEqualTo(0)
    assertThat(memory.heapMaxBytes).isGreaterThan(0)
    assertThat(memory.heapUsedBeforeBytes).isGreaterThan(0)
    assertThat(memory.gcCollections).isGreaterThanOrEqualTo(0)
    assertThat(memory.gcTimeMillis).isGreaterThanOrEqualTo(0)
  }
}
