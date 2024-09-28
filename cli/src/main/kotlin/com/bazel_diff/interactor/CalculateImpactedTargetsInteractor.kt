package com.bazel_diff.interactor

import com.bazel_diff.hash.TargetHash
import com.google.common.collect.Maps
import org.koin.core.component.KoinComponent
import java.io.File
import java.util.concurrent.ConcurrentMap
import java.util.concurrent.ConcurrentHashMap
import java.util.stream.Stream
import java.util.stream.Collectors
import com.google.common.annotations.VisibleForTesting

data class TargetDistanceMetrics(
    val targetDistance: Int,
    val packageDistance: Int
) {}

class CalculateImpactedTargetsInteractor : KoinComponent {

    @VisibleForTesting
    class InvalidDependencyEdgesException(message: String) : Exception(message)

    enum class ImpactType {
    DIRECT,
    INDIRECT
    }

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

    fun executeWithDistances(from: Map<String, TargetHash>, to: Map<String, TargetHash>, depEdges: Map<String, List<String>>): Map<String, TargetDistanceMetrics> {
        val difference = Maps.difference(to, from)

        val newLabels = difference.entriesOnlyOnLeft().keys
        val existingImpactedLabels = difference.entriesDiffering().keys

        val impactedLabels = HashMap<String, ImpactType>().apply {
            newLabels.forEach { this[it] = ImpactType.DIRECT }
            existingImpactedLabels.forEach { 
                this[it] = if (from[it]!!.directHash != to[it]!!.directHash) ImpactType.DIRECT else ImpactType.INDIRECT 
            }
        }

        val computedResult: ConcurrentMap<String, TargetDistanceMetrics> = ConcurrentHashMap()

        impactedLabels.keys.parallelStream()
            .forEach {
                calculateDistance(it, depEdges, computedResult, impactedLabels)
            }

        return computedResult
    }

    fun calculateDistance(
            label: String,
            depEdges: Map<String, List<String>>,
            impactedTargets: ConcurrentMap<String, TargetDistanceMetrics>,
            impactedLabels: Map<String, ImpactType>
            ): TargetDistanceMetrics {
        impactedTargets[label]?.let { return it }

        if (label !in impactedLabels) {
            throw IllegalArgumentException("$label was not impacted, but was requested to calculate distance.")
        }

        // If the label is directly impacted, it has a distance of 0
        if (impactedLabels[label] == ImpactType.DIRECT) {
            return TargetDistanceMetrics(0, 0).also { impactedTargets[label] = it }
        }

        val directDeps = depEdges[label] ?: throw InvalidDependencyEdgesException("$label was indirectly impacted, but has no dependencies.")

        // Now compute the distance for label, which was indirectly impacted
        val (targetDistance, packageDistance) = directDeps.parallelStream()
            .filter {it in impactedLabels}
            .collect(Collectors.toList())
            .let { impactedDepLabels ->
                if (impactedDepLabels.isEmpty()) {
                    throw InvalidDependencyEdgesException("$label was indirectly impacted, but has no impacted dependencies.")
                }
                impactedDepLabels.parallelStream()
            }.map { dep ->
                val distanceMetrics = calculateDistance(dep, depEdges, impactedTargets, impactedLabels)
                val crossesPackageBoundary = label.split(":")[0] != dep.split(":")[0]
                Pair(   
                    distanceMetrics.targetDistance + 1,
                    distanceMetrics.packageDistance + if (crossesPackageBoundary) 1 else 0
                )
            }
            .collect(Collectors.toList())
            .let { distances ->
                val minTargetDistance = distances.minOf { it.first }
                val minPackageDistance = distances.minOf { it.second }
                Pair(minTargetDistance, minPackageDistance)
            }

        
        return TargetDistanceMetrics(targetDistance, packageDistance).also { impactedTargets[label] = it }
    }
}
