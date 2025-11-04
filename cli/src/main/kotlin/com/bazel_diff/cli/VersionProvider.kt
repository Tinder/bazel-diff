package com.bazel_diff.cli

import java.io.BufferedReader
import java.io.InputStreamReader
import picocli.CommandLine.IVersionProvider

class VersionProvider : IVersionProvider {
  override fun getVersion(): Array<String> {
    val classLoader = this::class.java.classLoader
    val inputStream =
        classLoader.getResourceAsStream("cli/version")
            ?: classLoader.getResourceAsStream("version")
            ?: throw IllegalArgumentException(
                "unknown version as version file not found in resources")

    val version = BufferedReader(InputStreamReader(inputStream)).use { it.readText().trim() }
    return arrayOf(version)
  }
}
