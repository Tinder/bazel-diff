package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isTrue
import org.junit.Assert.assertThrows
import org.junit.Test
import picocli.CommandLine

class BazelDiffTest {
  @Test
  fun runThrowsWhenInvokedWithoutSubcommand() {
    // BazelDiff is a parent command -- it must be invoked with `generate-hashes` or
    // `get-impacted-targets`. Calling `run()` directly mirrors picocli's behaviour
    // when the user passes no subcommand and should surface a ParameterException
    // that picocli will translate to a usage error.
    val diff = BazelDiff()
    diff.spec = CommandLine(diff).commandSpec
    val ex = assertThrows(CommandLine.ParameterException::class.java) { diff.run() }
    assertThat(ex.message!!).contains("Missing required subcommand")
  }

  @Test
  fun isVerboseFalseByDefault() {
    assertThat(BazelDiff().isVerbose()).isFalse()
  }

  @Test
  fun isVerboseTrueWhenVerboseFlagSet() {
    val diff = BazelDiff()
    diff.verbose = true
    assertThat(diff.isVerbose()).isTrue()
  }

  @Test
  fun isVerboseTrueWhenDebugFlagSet() {
    val diff = BazelDiff()
    diff.debug = true
    assertThat(diff.isVerbose()).isTrue()
  }

  // --alwaysAffectedTags (issue #401) parses into a comma-separated Set on generate-hashes.
  @Test
  fun generateHashesParsesAlwaysAffectedTags() {
    val parseResult =
        CommandLine(BazelDiff())
            .parseArgs(
                "generate-hashes",
                "-w",
                "/tmp/ws",
                "--alwaysAffectedTags=external,no-cache",
                "/tmp/out.json")
    val command = parseResult.subcommand().commandSpec().userObject() as GenerateHashesCommand
    assertThat(command.alwaysAffectedTags).isEqualTo(setOf("external", "no-cache"))
  }

  @Test
  fun generateHashesAlwaysAffectedTagsDefaultsToEmpty() {
    val parseResult =
        CommandLine(BazelDiff()).parseArgs("generate-hashes", "-w", "/tmp/ws", "/tmp/out.json")
    val command = parseResult.subcommand().commandSpec().userObject() as GenerateHashesCommand
    assertThat(command.alwaysAffectedTags).isEmpty()
  }
}
