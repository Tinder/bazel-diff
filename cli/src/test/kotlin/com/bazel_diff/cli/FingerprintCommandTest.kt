package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEqualTo
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.PrintStream
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import picocli.CommandLine

class FingerprintCommandTest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private fun fakeBazel(label: String): File =
      File(temp.root, "bazel").apply {
        writeText("#!/bin/sh\necho '$label'\n")
        setExecutable(true)
      }

  private fun command(ws: File): FingerprintCommand =
      FingerprintCommand().apply {
        workspacePath = ws.toPath()
        bazelPath = fakeBazel("Build label: 8.5.1").toPath()
      }

  @Test
  fun writesFingerprintJsonToFile() {
    val ws = temp.newFolder("ws")
    File(ws, ".bazelrc").writeText("common --x")
    val out = File(temp.root, "fp.json")
    val cmd = command(ws).apply { outputPath = out }

    assertThat(cmd.call()).isEqualTo(CommandLine.ExitCode.OK)
    val json = out.readText()
    assertThat(json).contains("\"fingerprint\"")
    assertThat(json).contains("\"flags\"")
    assertThat(json).contains("\"components\"")
  }

  @Test
  fun fingerprintChangesWhenAFlagChanges() {
    val ws = temp.newFolder("ws2")
    val a = File(temp.root, "a.json")
    val b = File(temp.root, "b.json")
    command(ws).apply { outputPath = a }.call()
    command(ws)
        .apply {
          outputPath = b
          useCquery = true
        }
        .call()
    // different flag set -> different fingerprint line
    val fpA = a.readLines().first { it.contains("\"fingerprint\"") }
    val fpB = b.readLines().first { it.contains("\"fingerprint\"") }
    assert(fpA != fpB) { "fingerprint should change when --useCquery changes" }
  }

  @Test
  fun dashOutputDoesNotCreateAFileNamedDash() {
    val ws = temp.newFolder("ws3")
    val cmd = command(ws).apply { outputPath = File("-") }
    assertThat(cmd.call()).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(File("-").exists()).isEqualTo(false)
  }

  @Test
  fun nullOutputPathWritesFingerprintJsonToStdout() {
    val ws = temp.newFolder("ws4")
    val cmd = command(ws).apply { outputPath = null }
    val captured = ByteArrayOutputStream()
    val previous = System.out
    System.setOut(PrintStream(captured))
    try {
      assertThat(cmd.call()).isEqualTo(CommandLine.ExitCode.OK)
    } finally {
      System.setOut(previous)
    }
    val json = captured.toString()
    assertThat(json).contains("\"fingerprint\"")
    assertThat(json).contains("\"flags\"")
  }

  @Test
  fun flagsInOutputAreSortedByKey() {
    val ws = temp.newFolder("ws5")
    val out = File(temp.root, "sorted.json")
    command(ws)
        .apply {
          outputPath = out
          keepGoing = true
          useCquery = true
          includeTargetType = true
          excludeExternalTargets = true
          bazelCommandOptions = listOf("--foo")
          bazelStartupOptions = listOf("--bar")
        }
        .call()
    val json = out.readText()
    // canonicalizeFlags + toSortedMap should emit keys in lexicographic order
    val flagsIdx = json.indexOf("\"flags\"")
    val keepGoingIdx = json.indexOf("\"keepGoing\"", flagsIdx)
    val useCqueryIdx = json.indexOf("\"useCquery\"", flagsIdx)
    assert(keepGoingIdx in 1 until useCqueryIdx) {
      "expected keepGoing before useCquery in sorted flags block"
    }
  }
}
