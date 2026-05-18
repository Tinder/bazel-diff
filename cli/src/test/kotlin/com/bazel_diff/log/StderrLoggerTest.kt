package com.bazel_diff.log

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEmpty
import java.io.ByteArrayOutputStream
import java.io.PrintStream
import org.junit.After
import org.junit.Before
import org.junit.Test

class StderrLoggerTest {
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
  fun errorMessageIsAlwaysWritten() {
    // `e(block)` does not gate on verbose; errors always reach stderr so a CLI user
    // sees the failure reason even with logging otherwise muted.
    StderrLogger(verbose = false).e { "boom" }
    assertThat(captured.toString()).contains("[Error] boom")
  }

  @Test
  fun errorWithThrowableWritesMessageAndStackTrace() {
    StderrLogger(verbose = false).e(RuntimeException("kaboom")) { "boom" }
    val out = captured.toString()
    assertThat(out).contains("[Error] boom")
    // Stack trace surfaces the exception message verbatim.
    assertThat(out).contains("kaboom")
  }

  @Test
  fun warningIsSuppressedWhenNotVerbose() {
    StderrLogger(verbose = false).w { "ignored" }
    assertThat(captured.toString()).isEmpty()
  }

  @Test
  fun warningIsEmittedWhenVerbose() {
    StderrLogger(verbose = true).w { "shown" }
    assertThat(captured.toString()).contains("[Warning] shown")
  }

  @Test
  fun infoIsSuppressedWhenNotVerbose() {
    StderrLogger(verbose = false).i { "ignored" }
    assertThat(captured.toString()).isEmpty()
  }

  @Test
  fun infoIsEmittedWhenVerbose() {
    StderrLogger(verbose = true).i { "shown" }
    assertThat(captured.toString()).contains("[Info] shown")
  }
}
