package com.bazel_diff.hash

import com.google.common.cache.CacheBuilder
import com.google.common.cache.CacheLoader
import org.koin.core.component.KoinComponent
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths

class ExternalRepoResolver(
        private val workingDirectory: Path,
        private val bazelPath: Path,
        private val outputBase: Path,
) : KoinComponent {
    private val externalRoot: Path by lazy {
        outputBase.resolve("external")
    }

    private val cache = CacheBuilder.newBuilder().build(CacheLoader.from { repoName: String ->
        val externalRepoRoot = externalRoot.resolve(repoName)
        if (Files.exists(externalRepoRoot)) {
            return@from externalRepoRoot
        }
        resolveBzlModPath(repoName)
    })

    fun resolveExternalRepoRoot(repoName: String): Path {
        return cache.get(repoName)
    }

    private fun resolveBzlModPath(repoName: String): Path {
        // Query result line should look something like "<exec root>/external/<canonical repo name>/some/bazel/target: <kind> <label>"
        val queryResultLine = runProcessAndCaptureFirstLine(bazelPath.toString(), "query", "@$repoName//...", "--output", "location")
        val path = Paths.get(queryResultLine.split(": ", limit = 2)[0])
        val bzlModRelativePath = path.relativize(externalRoot).first()
        return externalRoot.resolve(bzlModRelativePath)
    }

    private fun runProcessAndCaptureFirstLine(vararg command: String): String {
        val process = ProcessBuilder(*command).directory(workingDirectory.toFile()).start()
        process.inputStream.bufferedReader().use {
            // read the first line and close the stream so that Bazel doesn't need to continue
            // output all the query result.
            return it.readLine()
        }
    }
}