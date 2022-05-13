package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.Path

class BazelQueryService(
    private val workingDirectory: Path,
    private val bazelPath: Path,
    private val startupOptions: List<String>,
    private val commandOptions: List<String>,
    private val keepGoing: Boolean?,
    private val debug: Boolean,
) : KoinComponent {
    private val logger: Logger by inject()

    @OptIn(ExperimentalCoroutinesApi::class)
    suspend fun query(query: String): List<Build.Target> {
        val tempFile = Files.createTempFile(null, ".txt")
        val outputFile = Files.createTempFile(null, ".bin")
        Files.write(tempFile, query.toByteArray(StandardCharsets.UTF_8))
        logger.i { "Executing Query: $query" }

        val cmd: MutableList<String> = ArrayList<String>().apply {
            add(bazelPath.toString())
            if (debug) {
                add("--bazelrc=/dev/null")
            }
            addAll(startupOptions)
            add("query")
            add("--output")
            add("streamed_proto")
            add("--order_output=no")
            if (keepGoing != null && keepGoing) {
                add("--keep_going")
            }
            addAll(commandOptions)
            add("--query_file")
            add(tempFile.toString())
        }

        val result = runBlocking {
            process(
                *cmd.toTypedArray(),
                stdout = Redirect.ToFile(outputFile.toFile()),
                workingDirectory = workingDirectory.toFile(),
                stderr = Redirect.PRINT,
                destroyForcibly = true,
            )
        }

        if(result.resultCode != 0) throw RuntimeException("Bazel query failed, exit code ${result.resultCode}")

        val targets = mutableListOf<Build.Target>()
        outputFile.toFile().inputStream().buffered().use {stream ->
            while (true) {
                val target = Build.Target.parseDelimitedFrom(stream) ?: break
                // EOF
                targets.add(target)
            }
        }

        Files.delete(tempFile)
        Files.delete(outputFile)
        return targets
    }
}
