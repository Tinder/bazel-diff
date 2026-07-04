package com.bazel_diff.log

import assertk.assertThat
import assertk.assertions.doesNotContain
import assertk.assertions.isNotNull
import java.io.ByteArrayOutputStream
import java.io.PrintStream
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.slf4j.LoggerFactory

/**
 * Regression test for the SLF4J warning printed by `bazel-diff serve`:
 * ```
 * SLF4J: Failed to load class "org.slf4j.impl.StaticLoggerBinder".
 * SLF4J: Defaulting to no-operation (NOP) logger implementation
 * ```
 *
 * JGit (the serve command's in-process git engine) depends on `slf4j-api` but ships no binding.
 * With no `org.slf4j.impl.StaticLoggerBinder` on the classpath, SLF4J falls back to a NOP logger
 * and prints the warning above to stderr on first use. Shipping `slf4j-nop` as a runtime dep
 * supplies the binding, silencing the warning while discarding JGit's internal logs (keeping the
 * CLI stderr clean).
 */
class Slf4jBindingTest {
  private val originalErr = System.err
  private lateinit var captured: ByteArrayOutputStream

  @Before
  fun setUp() {
    captured = ByteArrayOutputStream()
    System.setErr(PrintStream(captured))
  }

  @After
  fun tearDown() {
    System.setErr(originalErr)
  }

  @Test
  fun staticLoggerBinderIsOnTheClasspath() {
    // This is the exact class SLF4J fails to load when no binding is present ("Failed to load class
    // org.slf4j.impl.StaticLoggerBinder"). If it resolves, the NOP-fallback warning cannot be
    // printed — a deterministic guard that survives SLF4J's once-per-JVM static init ordering.
    val binder = Class.forName("org.slf4j.impl.StaticLoggerBinder")
    assertThat(binder).isNotNull()
  }

  @Test
  fun obtainingAndUsingALoggerDoesNotPrintNopFallbackWarning() {
    // Exercises the same path JGit takes: resolve a logger and emit a record. With a binding
    // present
    // this is silent; without one, SLF4J's static init prints the warning to stderr.
    val logger = LoggerFactory.getLogger(Slf4jBindingTest::class.java)
    assertThat(logger).isNotNull()
    logger.info("bazel-diff slf4j binding smoke test")

    val err = captured.toString()
    assertThat(err).doesNotContain("StaticLoggerBinder")
    assertThat(err).doesNotContain("NOP logger")
  }
}
