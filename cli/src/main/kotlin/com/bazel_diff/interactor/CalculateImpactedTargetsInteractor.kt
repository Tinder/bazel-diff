package com.bazel_diff.interactor

import com.bazel_diff.hash.TargetHash
import com.google.common.collect.Maps
import org.koin.core.component.KoinComponent
import java.io.File

class CalculateImpactedTargetsInteractor : KoinComponent {
    fun execute(from: Map<String, TargetHash>, to: Map<String, TargetHash>): Set<String> {
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
        return impactedTargets
    }
}
