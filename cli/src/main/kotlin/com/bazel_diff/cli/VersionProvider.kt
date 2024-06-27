package com.bazel_diff.cli

import picocli.CommandLine.IVersionProvider
import java.io.BufferedReader
import java.io.InputStreamReader

class VersionProvider : IVersionProvider {
    override fun getVersion(): Array<String> {
        val classLoader = this::class.java.classLoader
        val inputStream = classLoader.getResourceAsStream(".bazelversion")
            ?: throw IllegalArgumentException("unknown version as .bazelversion file not found in resources")

        val version = BufferedReader(InputStreamReader(inputStream)).use { it.readText().trim() }
        return arrayOf(version)
    }
}
