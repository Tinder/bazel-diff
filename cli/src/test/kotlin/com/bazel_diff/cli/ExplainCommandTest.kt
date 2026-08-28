package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.PrintStream
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import picocli.CommandLine

/**
 * Drives the command through the real picocli parse so `--format`, defaults, and the parent
 * command's `--verbose` inheritance are all exercised the way a user hits them.
 */
class ExplainCommandTest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  // //common:util.py -> //common:lib -> //service/a:app
  private fun write(name: String, contents: String): File =
      File(temp.root, name).apply { writeText(contents) }

  private fun fromHashes() =
      write(
          "from.json",
          """
          {
            "//service/a:app": "Rule#app~app-direct",
            "//common:lib": "Rule#lib~lib-direct",
            "//common:util.py": "SourceFile#util~util"
          }
          """
              .trimIndent())

  private fun toHashes() =
      write(
          "to.json",
          """
          {
            "//service/a:app": "Rule#app2~app-direct",
            "//common:lib": "Rule#lib2~lib-direct",
            "//common:util.py": "SourceFile#util2~util2"
          }
          """
              .trimIndent())

  private fun depEdges() =
      write(
          "deps.json",
          """
          {
            "//service/a:app": ["//common:lib"],
            "//common:lib": ["//common:util.py"]
          }
          """
              .trimIndent())

  private fun execute(vararg extraArgs: String): Int =
      CommandLine(BazelDiff())
          .execute(
              "explain",
              "-sh",
              fromHashes().absolutePath,
              "-fh",
              toHashes().absolutePath,
              "-d",
              depEdges().absolutePath,
              "-t",
              "//service/a:app",
              *extraArgs)

  /**
   * Runs `explain` and returns its rendered output.
   *
   * Always goes through `-o`: the command writes STDOUT via `FileWriter(FileDescriptor.out)` (as
   * every other bazel-diff command does), which addresses file descriptor 1 directly and so is
   * invisible to `System.setOut`. [writesToStdoutWhenNoOutputPathIsGiven] covers the STDOUT branch.
   */
  private fun run(vararg extraArgs: String): Pair<Int, String> {
    val out = File(temp.root, "explain-output")
    val exit = execute(*extraArgs, "-o", out.absolutePath)
    return Pair(exit, if (out.exists()) out.readText() else "")
  }

  @Test
  fun defaultsToTextAndNamesTheUpstreamRootCause() {
    val (exit, out) = run()

    assertThat(exit).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(out).contains("//service/a:app")
    assertThat(out).contains("IMPACTED (indirectly")
    assertThat(out).contains("//common:util.py")
    assertThat(out).contains("//common:util.py -> //common:lib -> //service/a:app")
  }

  @Test
  fun writesTheChosenFormatToTheOutputFile() {
    val out = File(temp.root, "graph.dot")

    val exit = execute("--format", "DOT", "-o", out.absolutePath)

    assertThat(exit).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(out.readText()).contains("digraph bazel_diff_impact {")
    assertThat(out.readText()).contains("(root cause)")
  }

  @Test
  fun writesToStdoutWhenNoOutputPathIsGiven() {
    // The default STDOUT branch: assert it completes cleanly and creates no stray file. The bytes
    // themselves go to fd 1 and are covered by the -o tests above.
    assertThat(execute()).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(File(temp.root, "explain-output").exists()).isEqualTo(false)
  }

  @Test
  fun rendersMermaid() {
    val (exit, out) = run("--format", "MERMAID")

    assertThat(exit).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(out).contains("flowchart TD")
    assertThat(out).contains("classDef rootCause")
  }

  @Test
  fun rendersJson() {
    val (exit, out) = run("--format", "JSON")

    assertThat(exit).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(out).contains("\"rootCauses\"")
    assertThat(out).contains("\"totalRootCauses\"")
    assertThat(out).contains("\"packageHops\"")
  }

  @Test
  fun maxDepthIsHonoured() {
    // util.py is 2 hops up; a 1-hop budget cannot reach it, so nothing is attributed.
    val (exit, out) = run("--maxDepth", "1")

    assertThat(exit).isEqualTo(CommandLine.ExitCode.OK)
    assertThat(out).contains("No root cause could be attributed")
  }

  @Test
  fun usageErrorForALabelInNeitherHashFile() {
    val originalErr = System.err
    val captured = ByteArrayOutputStream()
    System.setErr(PrintStream(captured, true))
    val exit =
        try {
          CommandLine(BazelDiff())
              .execute(
                  "explain",
                  "-sh",
                  fromHashes().absolutePath,
                  "-fh",
                  toHashes().absolutePath,
                  "-d",
                  depEdges().absolutePath,
                  "-t",
                  "//typo:nope")
        } finally {
          System.setErr(originalErr)
        }

    assertThat(exit).isEqualTo(CommandLine.ExitCode.USAGE)
    assertThat(captured.toString()).contains("is not present in either hash file")
  }

  @Test
  fun usageErrorForAnUnreadableInputFile() {
    val originalErr = System.err
    val captured = ByteArrayOutputStream()
    System.setErr(PrintStream(captured, true))
    val exit =
        try {
          CommandLine(BazelDiff())
              .execute(
                  "explain",
                  "-sh",
                  File(temp.root, "missing.json").absolutePath,
                  "-fh",
                  toHashes().absolutePath,
                  "-d",
                  depEdges().absolutePath,
                  "-t",
                  "//service/a:app")
        } finally {
          System.setErr(originalErr)
        }

    assertThat(exit).isEqualTo(CommandLine.ExitCode.USAGE)
    assertThat(captured.toString()).contains("Incorrect starting hashes")
  }

  @Test
  fun usageErrorForAnUnreadableDepEdgesFile() {
    val originalErr = System.err
    val captured = ByteArrayOutputStream()
    System.setErr(PrintStream(captured, true))
    val exit =
        try {
          CommandLine(BazelDiff())
              .execute(
                  "explain",
                  "-sh",
                  fromHashes().absolutePath,
                  "-fh",
                  toHashes().absolutePath,
                  "-d",
                  File(temp.root, "missing-deps.json").absolutePath,
                  "-t",
                  "//service/a:app")
        } finally {
          System.setErr(originalErr)
        }

    assertThat(exit).isEqualTo(CommandLine.ExitCode.USAGE)
    assertThat(captured.toString()).contains("Incorrect dep edges file")
  }

  @Test
  fun explainIsRegisteredAsASubcommand() {
    val subcommands = CommandLine(BazelDiff()).subcommands.keys
    assertThat(subcommands.contains("explain")).isEqualTo(true)
  }

  @Test
  fun maxRootCausesDefaultsTo25() {
    val parsed =
        CommandLine(BazelDiff())
            .parseArgs(
                "explain",
                "-sh",
                "/tmp/a.json",
                "-fh",
                "/tmp/b.json",
                "-d",
                "/tmp/d.json",
                "-t",
                "//a:b")
    val command = parsed.subcommand().commandSpec().userObject() as ExplainCommand
    assertThat(command.maxRootCauses).isEqualTo(25)
    assertThat(command.maxDepth).isEqualTo(-1)
    assertThat(command.format).isNotEqualTo(null)
  }
}
