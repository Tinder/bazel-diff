package com.bazel_diff.process

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.Ignore
import org.junit.Test
import kotlin.test.assertNull

/**
 * Reproducer for https://github.com/Tinder/bazel-diff/issues/256
 * ("bug: bazel-diff freezes when aspect CLI is installed").
 *
 * The user reported that with `.bazeliskrc` set to download aspect CLI
 * (`USE_BAZEL_VERSION=aspect/...`), every `bazel-diff` invocation hangs forever.
 * An `strace` on the Java process showed it parked in a `FUTEX_WAIT`, i.e. waiting
 * on the bazel subprocess that never makes progress. The cross-posted aspect-cli
 * issue (aspect-build/aspect-cli-legacy#41) corroborates this and Alex Eagle
 * suggested unsetting `BAZELISK_BASE_URL` / `USE_BAZEL_VERSION` as a workaround.
 *
 * Likely root cause exercised by this test: [process] starts the subprocess via
 * `ProcessBuilder.start()` without redirecting stdin. Java's default for stdin is
 * `Redirect.PIPE`, so the subprocess receives an open, never-closed stdin pipe.
 * Any subprocess that reads from stdin (e.g., the aspect CLI's interactive
 * "first run" path) blocks indefinitely waiting for input that bazel-diff never
 * sends, and `process.waitFor()` then blocks forever.
 *
 * This test reproduces the pattern with a minimal subprocess (`cat`) that simply
 * reads from stdin. It is marked `@Ignore` because, by design, the test would
 * either deadlock CI or assert-on-a-timeout — neither is desirable in the
 * regularly-scheduled test suite. Removing `@Ignore` and watching the
 * `withTimeoutOrNull` return non-null is the expected signal that a fix
 * (e.g., `redirectInput(Redirect.from(/dev/null))` or
 * `process.outputStream.close()` after `start()`) is in place.
 *
 * The follow-up fix is intentionally out of scope for this PR; see issue #256
 * for the discussion.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class ProcessStdinHangReproducerTest {

  @Ignore("Reproducer for #256 — proves process() deadlocks when the subprocess reads from stdin.")
  @Test
  fun `process hangs when subprocess reads from stdin and parent never closes it`() = runBlocking {
    // `cat` with no args reads from stdin until EOF. Because process() never
    // closes the subprocess's stdin pipe, `cat` blocks on read forever, never
    // writes anything to stdout, and `process.waitFor()` (and the stdout
    // line-flow that precedes it) blocks forever.
    val result =
        withTimeoutOrNull(timeMillis = 5_000) {
          process(
              "cat",
              stdout = Redirect.CAPTURE,
              stderr = Redirect.SILENT,
          )
        }

    // `null` here means the timeout fired, i.e. process() hung — the bug is
    // reproduced. If/when the underlying behaviour is fixed (stdin is closed
    // or redirected to /dev/null), `cat` will see EOF immediately, exit with
    // status 0, and `result` will be a populated `ProcessResult`. Flip this
    // assertion (and drop `@Ignore`) once the fix lands.
    assertNull(result, "Expected process() to deadlock; if it returned, #256 may be fixed.")
  }
}
