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
import java.util.Calendar

class GenerateHashesInteractor : KoinComponent {
    private val buildGraphHasher: BuildGraphHasher by inject()
    private val logger: Logger by inject()
    private val gson: Gson by inject()

    fun execute(seedFilepaths: File?, outputPath: File?, depsMappingJSONPath: File?, ignoredRuleHashingAttributes: Set<String>, targetTypes:Set<String>?, includeTargetType: Boolean = false, modifiedFilepaths: File?): Boolean {
        return try {
            val epoch = Calendar.getInstance().getTimeInMillis()
            val seedFilepathsSet: Set<Path> = when {
                seedFilepaths != null -> {
                    BufferedReader(FileReader(seedFilepaths)).use {
                        it.readLines()
                            .map { line: String -> File(line).toPath() }
                            .toSet()
                    }
                }
                else -> emptySet()
            }
            var modifiedFilepathsSet: Set<Path> = when {
                modifiedFilepaths != null -> {
                    BufferedReader(FileReader(modifiedFilepaths)).use {
                        it.readLines()
                            .map { line: String -> File(line).toPath() }
                            .toSet()
                    }
                }
                else -> emptySet()
            }
            val hashes = buildGraphHasher.hashAllBazelTargetsAndSourcefiles(
                seedFilepathsSet,
                ignoredRuleHashingAttributes,
                modifiedFilepathsSet
            ).let {
                if(targetTypes == null) {
                    it
                } else {
                    it.filter { targetTypes.contains(it.value.type) }
                }
            }
            when (outputPath) {
                null -> FileWriter(FileDescriptor.out)
                else -> FileWriter(outputPath)
            }.use { fileWriter ->
                fileWriter.write(gson.toJson(hashes.mapValues { it.value.toJson(includeTargetType) }))
            }
            if (depsMappingJSONPath != null) {
                FileWriter(depsMappingJSONPath).use { fileWriter ->
                    fileWriter.write(gson.toJson(hashes.mapValues { it.value.deps }))
                }
            }
            val duration = Calendar.getInstance().getTimeInMillis() - epoch;
            logger.i { "generate-hashes finished in $duration" }
            true
        } catch (e: Exception) {
            logger.e(e) { "Unexpected error during generation of hashes" }
            false
        }
    }
}
