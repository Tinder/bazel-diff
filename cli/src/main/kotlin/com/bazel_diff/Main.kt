package com.bazel_diff

import com.bazel_diff.cli.BazelDiff
import kotlin.system.exitProcess
import picocli.CommandLine

class Main {
  companion object {
    @JvmStatic
    fun main(args: Array<String>) {
      val exitCode = CommandLine(BazelDiff()).execute(*args)
      exitProcess(exitCode)
    }
  }
}
