package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEqualTo
import java.io.File
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class WarmupCommandTest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private fun fakeBazel(label: String): File =
      File(temp.root, "bazel").apply {
        writeText("#!/bin/sh\necho '$label'\n")
        setExecutable(true)
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
}
