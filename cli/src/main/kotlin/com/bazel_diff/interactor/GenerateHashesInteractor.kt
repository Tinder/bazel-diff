package com.bazel_diff.interactor

import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.log.Logger
import com.google.gson.Gson
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.io.BufferedReader
import java.io.File
import java.io.FileDescriptor
import java.io.FileReader
import java.io.FileWriter
import java.nio.file.Path
import kotlin.time.ExperimentalTime
import kotlin.time.measureTime

class GenerateHashesInteractor : KoinComponent {
    private val buildGraphHasher: BuildGraphHasher by inject()
    private val logger: Logger by inject()
    private val gson: Gson by inject()

    @OptIn(ExperimentalTime::class)
    fun execute(seedFilepaths: File?, outputPath: File?): Boolean {
        return try {
            val duration = measureTime {
                var seedFilepathsSet: Set<Path> = when {
                    seedFilepaths != null -> {
                        BufferedReader(FileReader(seedFilepaths)).use {
                            it.readLines()
                                .map { line: String -> File(line).toPath() }
                                .toSet()
                        }
                    }
                    else -> emptySet()
                }
                val hashes = buildGraphHasher.hashAllBazelTargetsAndSourcefiles(seedFilepathsSet)
                when (outputPath) {
                    null -> FileWriter(FileDescriptor.out)
                    else -> FileWriter(outputPath)
                }.use {
                    it.write(gson.toJson(hashes))
                }
            }
            logger.i { "generate-hashes finished in $duration" }
            true
        } catch (e: Exception) {
            logger.e(e) { "Unexpected error during generation of hashes" }
            false
        }
    }
}
