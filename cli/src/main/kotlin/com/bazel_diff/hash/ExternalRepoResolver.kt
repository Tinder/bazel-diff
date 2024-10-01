package com.bazel_diff.hash

import com.bazel_diff.log.Logger
import com.google.common.cache.CacheBuilder
import com.google.common.cache.CacheLoader
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths

class ExternalRepoResolver(
    private val workingDirectory: Path,
    private val bazelPath: Path,
    private val outputBase: Path,
) : KoinComponent {
    private val logger: Logger by inject()

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
        val queryResultLine = runProcessAndCaptureFirstLine(
            bazelPath.toString(),
            "query",
            "@$repoName//...",
            "--keep_going",
            "--output",
            "location"
        )
        if (queryResultLine == null) {
            // Fallback to the default external repo root if nothing was found via bazel query.
            // Normally this should not happen since any external repo should have at least one
            // target to be consumed. But if it does, we just return the default external repo root
            // and caller would handle the non-existent path.
            logger.w {
                "External repo $repoName has no target under it. You probably want to remove " +
                        "this repo from `fineGrainedHashExternalRepos` since bazel-diff is not " +
                        "able to correctly generate fine-grained hash for it."
            }
            return externalRoot.resolve(repoName);
        }
        val path = Paths.get(queryResultLine.split(": ", limit = 2)[0])
        val bzlModRelativePath = path.relativize(externalRoot).first()
        return externalRoot.resolve(bzlModRelativePath)
    }

    private fun runProcessAndCaptureFirstLine(vararg command: String): String? {
        val process = ProcessBuilder(*command).directory(workingDirectory.toFile()).start()
        process.inputStream.bufferedReader().use {
            // read the first line and close the stream so that Bazel doesn't need to continue
            // output all the query result.
            return it.readLine()
        }
    }
}
