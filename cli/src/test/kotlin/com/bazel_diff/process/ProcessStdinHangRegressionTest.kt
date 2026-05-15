package com.bazel_diff.process

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull

/**
 * Regression test for https://github.com/Tinder/bazel-diff/issues/256
 * ("bug: bazel-diff freezes when aspect CLI is installed").
 *
 * Before the fix, [process] started subprocesses via `ProcessBuilder.start()`
 * without redirecting or closing stdin. Java defaults stdin to `Redirect.PIPE`,
 * so the subprocess received an open, never-closed stdin pipe. Any subprocess
 * that reads from stdin (the aspect CLI's interactive first-run path, in #256)
 * blocked indefinitely on `read()`, and `process.waitFor()` then blocked too —
 * matching the `FUTEX_WAIT` strace the original reporter captured.
 *
 * The fix closes `process.outputStream` immediately after `start()` so the
 * subprocess sees EOF on stdin and exits.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class ProcessStdinHangRegressionTest {

  @Test
  fun `process does not hang when subprocess reads from stdin`() = runBlocking {
    // `cat` with no args reads from stdin until EOF. Before the fix this hung
    // forever; after the fix `cat` sees EOF immediately and exits 0.
    val result =
        withTimeoutOrNull(timeMillis = 5_000) {
          process(
              "cat",
              stdout = Redirect.CAPTURE,
              stderr = Redirect.SILENT,
          )
        }

    assertNotNull(result, "process() deadlocked — subprocess stdin was not closed (regression of #256).")
    assertEquals(0, result.resultCode)
  }
}
