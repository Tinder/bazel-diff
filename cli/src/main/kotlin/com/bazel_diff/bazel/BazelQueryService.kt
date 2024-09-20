package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import com.google.devtools.build.lib.analysis.AnalysisProtosV2
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.io.File
import java.nio.file.Files
import java.nio.file.Path

class BazelQueryService(
        private val workingDirectory: Path,
        private val bazelPath: Path,
        private val startupOptions: List<String>,
        private val commandOptions: List<String>,
        private val cqueryOptions: List<String>,
        private val keepGoing: Boolean,
        private val noBazelrc: Boolean,
) : KoinComponent {
    private val logger: Logger by inject()

    suspend fun query(
        query: String,
        useCquery: Boolean = false)
    : List<Build.Target> {
        // Unfortunately, there is still no direct way to tell if a target is compatible or not with the proto output
        // by itself. So we do an extra cquery with the trick at
        // https://bazel.build/extending/platforms#cquery-incompatible-target-detection to first find all compatible
        // targets.
        val compatibleTargetSet =
                if (useCquery) {
                    runQuery(query, useCquery = true, outputCompatibleTargets = true).useLines {
                        it.filter { it.isNotBlank() }.toSet()
                    }
                } else {
                    emptySet()
                }
        val outputFile = runQuery(query, useCquery)

        val targets = outputFile.inputStream().buffered().use { proto ->
            if (useCquery) {
                val cqueryResult = AnalysisProtosV2.CqueryResult.parseFrom(proto)
                cqueryResult.resultsList.filter { it.target.rule.name in compatibleTargetSet }.map { it.target }
            } else {
                mutableListOf<Build.Target>().apply {
                    while (true) {
                        val target = Build.Target.parseDelimitedFrom(proto) ?: break
                        // EOF
                        add(target)
                    }
                }
            }
        }

        return targets
    }

    @OptIn(ExperimentalCoroutinesApi::class)
    private suspend fun runQuery(
        query: String,
        useCquery: Boolean,
        outputCompatibleTargets: Boolean = false
    ): File {
        val queryFile = Files.createTempFile(null, ".txt").toFile()
        queryFile.deleteOnExit()
        val outputFile = Files.createTempFile(null, ".bin").toFile()
        outputFile.deleteOnExit()

        queryFile.writeText(query)

        val cmd: MutableList<String> = ArrayList<String>().apply {
            add(bazelPath.toString())
            if (noBazelrc) {
                add("--bazelrc=/dev/null")
            }
            addAll(startupOptions)
            if (useCquery) {
                add("cquery")
                if (!outputCompatibleTargets) {
                    // There is no need to query the transitions when querying for compatible targets.
                    add("--transitions=lite")
                }
            } else {
                add("query")
            }
            add("--output")
            if (useCquery) {
                if (outputCompatibleTargets) {
                    add("starlark")
                    add("--starlark:file")
                    val cqueryOutputFile = Files.createTempFile(null, ".cquery").toFile()
                    cqueryOutputFile.deleteOnExit()
                    cqueryOutputFile.writeText("""
                    def format(target):
                        if providers(target) == None:
                            # skip printing non-target results. That is, source files and generated files won't be
                            # printed
                            return ""
                        if "IncompatiblePlatformProvider" not in providers(target):
                            label = str(target.label)
                            # normalize label to be consistent with content inside proto
                            if label.startswith("@//"):
                                return label[1:]
                            if label.startswith("@@//"):
                                return label[2:]
                            return label
                        return ""
                    """.trimIndent())
                    add(cqueryOutputFile.toString())
                } else {
                    // Unfortunately, cquery does not support streamed_proto yet.
                    // See https://github.com/bazelbuild/bazel/issues/17743. This poses an issue for large monorepos.
                    add("proto")
                }
            } else {
                add("streamed_proto")
            }
            if (!useCquery) {
                add("--order_output=no")
            }
            if (keepGoing) {
                add("--keep_going")
            }
            if (useCquery) {
                addAll(cqueryOptions)
            } else {
                addAll(commandOptions)
            }
            add("--query_file")
            add(queryFile.toString())
        }

        logger.i { "Executing Query: $query" }
        logger.i { "Command: ${cmd.toTypedArray().joinToString()}" }
        val result = runBlocking {
            process(
                    *cmd.toTypedArray(),
                    stdout = Redirect.ToFile(outputFile),
                    workingDirectory = workingDirectory.toFile(),
                    stderr = Redirect.PRINT,
                    destroyForcibly = true,
            )
        }

        if (result.resultCode != 0)
            throw RuntimeException("Bazel query failed, exit code ${result.resultCode}")
        return outputFile
    }
}
