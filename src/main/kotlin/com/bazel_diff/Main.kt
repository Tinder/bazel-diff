package com.bazel_diff

import com.bazel_diff.cli.BazelDiff
import picocli.CommandLine
import kotlin.system.exitProcess

class Main {
    companion object {
        @JvmStatic
        fun main(args: Array<String>) {
            val exitCode = CommandLine(BazelDiff()).execute(*args)
            exitProcess(exitCode)
        }
    }
}
