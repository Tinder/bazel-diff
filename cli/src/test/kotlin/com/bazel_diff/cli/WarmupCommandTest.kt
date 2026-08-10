package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import java.io.File
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import picocli.CommandLine

class WarmupCommandTest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private fun fakeBazel(label: String): File =
      File(temp.root, "bazel").apply {
        writeText("#!/bin/sh\necho '$label'\n")
        setExecutable(true)
      }

  /**
   * Stubs [WarmupCommand.runGenerateHashes] so [WarmupCommand.call] can be covered without invoking
   * a real `bazel query`.
   */
  private class WarmupCommandUnderTest : WarmupCommand() {
    var generateHashesResult: Int = CommandLine.ExitCode.OK
    var generateHashesCalled = false

    override fun runGenerateHashes(): Int {
      generateHashesCalled = true
      return generateHashesResult
    }
  }

  private fun underTest(
      ws: File,
      baseHashes: File,
      fingerprint: File,
  ): WarmupCommandUnderTest =
      WarmupCommandUnderTest().apply {
        workspacePath = ws.toPath()
        bazelPath = fakeBazel("Build label: 8.5.1").toPath()
        baseHashesPath = baseHashes
        fingerprintOutputPath = fingerprint
      }

  @Test
  fun writeFingerprintEmitsJsonReflectingFlags() {
    val ws = temp.newFolder("ws")
    File(ws, ".bazelrc").writeText("common --x")
    val fpOut = File(temp.newFolder("snap"), "fingerprint.json")

    val cmd =
        WarmupCommand().apply {
          workspacePath = ws.toPath()
          bazelPath = fakeBazel("Build label: 8.5.1").toPath()
          fingerprintOutputPath = fpOut
        }
    cmd.writeFingerprint()

    val json = fpOut.readText()
    assertThat(json).contains("\"fingerprint\"")
    assertThat(json).contains("\"flags\"")
    // the flag set the fingerprint covers is rendered, reflecting defaults
    assertThat(json).contains("useCquery")
  }

  @Test
  fun writeFingerprintCreatesParentDirs() {
    val ws = temp.newFolder("ws2")
    // nested, not-yet-existing output dir must be created
    val fpOut = File(temp.root, "nested/dir/fingerprint.json")
    WarmupCommand()
        .apply {
          workspacePath = ws.toPath()
          bazelPath = fakeBazel("Build label: 8.5.1").toPath()
          fingerprintOutputPath = fpOut
        }
        .writeFingerprint()
    assertThat(fpOut.exists()).isEqualTo(true)
  }

  @Test
  fun fingerprintChangesWithFlags() {
    val ws = temp.newFolder("ws3")
    val a = File(temp.root, "a.json")
    val b = File(temp.root, "b.json")
    fun run(out: File, cquery: Boolean) =
        WarmupCommand()
            .apply {
              workspacePath = ws.toPath()
              bazelPath = fakeBazel("Build label: 8.5.1").toPath()
              fingerprintOutputPath = out
              useCquery = cquery
            }
            .writeFingerprint()
    run(a, false)
    run(b, true)
    val fpA = a.readLines().first { it.contains("\"fingerprint\"") }
    val fpB = b.readLines().first { it.contains("\"fingerprint\"") }
    assert(fpA != fpB) { "fingerprint must change when --useCquery changes" }
  }

  @Test
  fun callWritesFingerprintAfterSuccessfulGenerateHashes() {
    val ws = temp.newFolder("ws-call-ok")
    File(ws, ".bazelrc").writeText("common --x")
    val snap = temp.newFolder("snap-ok")
    val baseHashes = File(snap, "nested/base_hashes.json")
    val fingerprint = File(snap, "nested/fingerprint.json")
    val cmd = underTest(ws, baseHashes, fingerprint)

    assertThat(cmd.call()).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(cmd.generateHashesCalled).isEqualTo(true)
    assertThat(cmd.outputPath).isEqualTo(baseHashes)
    assertThat(baseHashes.parentFile.exists()).isEqualTo(true)
    assertThat(fingerprint.exists()).isEqualTo(true)
    assertThat(fingerprint.readText()).contains("\"fingerprint\"")
  }

  @Test
  fun callSkipsFingerprintWhenGenerateHashesFails() {
    val ws = temp.newFolder("ws-call-fail")
    val snap = temp.newFolder("snap-fail")
    val baseHashes = File(snap, "base_hashes.json")
    val fingerprint = File(snap, "fingerprint.json")
    val cmd =
        underTest(ws, baseHashes, fingerprint).apply {
          generateHashesResult = CommandLine.ExitCode.SOFTWARE
        }

    assertThat(cmd.call()).isEqualTo(CommandLine.ExitCode.SOFTWARE)
    assertThat(cmd.generateHashesCalled).isEqualTo(true)
    assertThat(fingerprint.exists()).isFalse()
  }
}
