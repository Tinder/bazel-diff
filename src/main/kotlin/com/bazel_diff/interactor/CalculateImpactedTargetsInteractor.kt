package com.bazel_diff.interactor

import com.bazel_diff.log.Logger
import com.google.common.collect.Maps
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.io.BufferedWriter
import java.io.File
import java.io.FileWriter
import java.io.IOException

class CalculateImpactedTargetsInteractor : KoinComponent {
    private val logger: Logger by inject()

    fun execute(from: Map<String, String>, to: Map<String, String>, outputPath: File): Boolean {
        /**
         * This call might be faster if end hashes is a sorted map
         */
        val difference = Maps.difference(to, from)
        val onlyInEnd: Set<String> = difference.entriesOnlyOnLeft().keys
        val changed: Set<String> = difference.entriesDiffering().keys
        val impactedTargets = HashSet<String>().apply {
            addAll(onlyInEnd)
            addAll(changed)
        }

        return try {
            BufferedWriter(FileWriter(outputPath)).use { writer ->
                impactedTargets.forEach {
                    writer.write(it)
                    //Should not be depend on OS
                    writer.write("\n")
                }
            }
            true
        } catch (e: IOException) {
            logger.e { "Unable to write to output filepath! Exiting!" }
            false
        }
    }
}
