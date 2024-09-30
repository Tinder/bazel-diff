package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.*
import com.bazel_diff.testModule
import org.junit.Rule
import org.junit.Test
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.mockito.junit.MockitoJUnit
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor.InvalidDependencyEdgesException

class CalculateImpactedTargetsInteractorTest : KoinTest {
    @get:Rule
    val mockitoRule = MockitoJUnit.rule()

    @get:Rule
    val koinTestRule = KoinTestRule.create {
        modules(testModule())
    }

    @Test
    fun testGetImpactedTargets() {
        val interactor = CalculateImpactedTargetsInteractor()
        val start: MutableMap<String, String> = HashMap()
        start["1"] = "a"
        start["2"] = "b"
        val startHashes = start.mapValues { TargetHash("", it.value, it.value) }

        val end: MutableMap<String, String> = HashMap()
        end["1"] = "c"
        end["2"] = "b"
        end["3"] = "d"
        val endHashes = end.mapValues { TargetHash("", it.value, it.value) }

        val impacted = interactor.computeSimpleImpactedTargets(startHashes, endHashes)
        assertThat(impacted).containsExactlyInAnyOrder(
            "1", "3"
        )
    }

    @Test
    fun testOmitsUnchangedTargets() {
        val (depEdges, startHashes) = createTargetHashes(
            "//:1 <- //:2 <- //:3",
            "//:unchanged <- //:3",
        )

        val endHashes = startHashes.toMutableMap()

        makeDirectlyChanged(endHashes, "//:1", "//:2")
        makeIndirectlyChanged(endHashes, "//:3")

        val interactor = CalculateImpactedTargetsInteractor()
        val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

        assertThat(impacted).containsOnly(
            "//:1" to TargetDistanceMetrics(0, 0),
            "//:2" to TargetDistanceMetrics(0, 0),
            "//:3" to TargetDistanceMetrics(1, 0),
        )
    }

    @Test
    fun testNewTargetsDirectlyModified() {
        val startHashes: Map<String, TargetHash> = mapOf()

        val endHashes = mapOf(
            "//:1" to TargetHash("", "a", "a"),
            "//:2" to TargetHash("", "b", "b"),
        )

        val interactor = CalculateImpactedTargetsInteractor()
        val impacted = interactor.computeAllDistances(startHashes, endHashes, mapOf())

        assertThat(impacted).containsOnly(
            "//:1" to TargetDistanceMetrics(0, 0),
            "//:2" to TargetDistanceMetrics(0, 0),
        )
    }

    @Test
    fun testTargetDistances() {
        var (depEdges, startHashes) = createTargetHashes(
            "//:1 <- //:2 <- //:3"
        )
        val endHashes = startHashes.toMutableMap()

        makeDirectlyChanged(endHashes, "//:1")
        makeIndirectlyChanged(endHashes, "//:2", "//:3")
        
        val interactor = CalculateImpactedTargetsInteractor()
        val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

        assertThat(impacted).containsOnly(
            "//:1" to TargetDistanceMetrics(0,0),
            "//:2" to TargetDistanceMetrics(1,0),
            "//:3" to TargetDistanceMetrics(2,0),
        )
    }

    @Test
    fun testPackageDistance() {
        var (depEdges, startHashes) = createTargetHashes(
            "//A:1 <- //A:2 <- //B:3 <- //B:4 <- //C:5"
        )
        val endHashes = startHashes.toMutableMap()

        makeDirectlyChanged(endHashes, "//A:1")
        makeIndirectlyChanged(endHashes, "//A:2", "//B:3", "//B:4", "//C:5")

        val interactor = CalculateImpactedTargetsInteractor()
        val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

        assertThat(impacted).containsOnly(
            "//A:1" to TargetDistanceMetrics(0,0),
            "//A:2" to TargetDistanceMetrics(1,0),
            "//B:3" to TargetDistanceMetrics(2,1),
            "//B:4" to TargetDistanceMetrics(3,1),
            "//C:5" to TargetDistanceMetrics(4,2),
        )
    }

    @Test
    fun testFindsShortestDistances() {
    	// Test that we find the shortest target and package distance, even if they each pass
        // through different dependency chains.
        //
        // //final:final's targetDistance should be 4 due to passing through //B, //C, and //D,
        // but its packageDistance should be 2 due to passing through //A.
        val (depEdges, startHashes) = createTargetHashes(
            "//changed:target <- //A:target_1 <- //A:target_2 <- //A:target_3 <- //A:target_4 <- //final:final",
            "//changed:target <- //B:target <- //C:target <- //D:target <- //final:final",
        )
        val endHashes = startHashes.toMutableMap()

        makeDirectlyChanged(endHashes, "//changed:target")
        makeIndirectlyChanged(endHashes, "//A:target_1", "//A:target_2", "//A:target_3", "//A:target_4", "//B:target", "//C:target", "//D:target", "//final:final")

        val interactor = CalculateImpactedTargetsInteractor()
        val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

        assertThat(impacted["//final:final"]).isEqualTo(TargetDistanceMetrics(4,2))
    }

    @Test
    fun testDependsOnNewTarget() {
        var (depEdges, startHashes) = createTargetHashes(
            "//:1 <- //:2 <- //:3"
        )
        val endHashes = startHashes.toMutableMap()

        // Remove //:1 so that it is a new target in the end hashes, but //:2 is
        // only indirectly modified.
        // Technically, this probably can't happen, as //:2 would have to have had
        // one of its deps attrs modified, which would have caused it to be directly
        // modified, but we should handle it anyway.
        startHashes.remove("//:1")

        makeIndirectlyChanged(endHashes, "//:2", "//:3")
        
        val interactor = CalculateImpactedTargetsInteractor()
        val impacted = interactor.computeAllDistances(startHashes, endHashes, depEdges)

        assertThat(impacted).containsOnly(
            "//:1" to TargetDistanceMetrics(0,0),
            "//:2" to TargetDistanceMetrics(1,0),
            "//:3" to TargetDistanceMetrics(2,0),
        )
    }

    @Test
    fun testInvalidEdgesRaises() {
        var (depEdges, startHashes) = createTargetHashes(
            "//:1 <- //:2"
        )
        val endHashes = startHashes.toMutableMap()

        makeIndirectlyChanged(endHashes, "//:2")

        val interactor = CalculateImpactedTargetsInteractor()
        assertThat {
            interactor.computeAllDistances(startHashes, endHashes, depEdges)
        }.isFailure().message().isEqualTo("//:2 was indirectly impacted, but has no impacted dependencies.")

        assertThat {
            // empty dep edges
            interactor.computeAllDistances(startHashes, endHashes, mapOf())
        }.isFailure().message().isEqualTo("//:2 was indirectly impacted, but has no dependencies.")

    }
        


    /**
     * Creates a mapping of target hashes and dependency edges from the provided graph specifications.
     *
     * @param graphSpecs Vararg parameter representing the graph specifications. Each specification is a string
     *                   where targets are separated by " <- " indicating a dependency relationship.
     * @return A pair consisting of:
     *         - A map where the keys are target labels and the values are lists of labels that depend on the key.
     *         - A map where the keys are target labels and the values are `TargetHash` objects representing the target hashes.
     */
    fun createTargetHashes(vararg graphSpecs: String): Pair<Map<String, List<String>>, MutableMap<String, TargetHash>> {
        val targetHashMap = mutableMapOf<String, TargetHash>()
        val depEdges = mutableMapOf<String, MutableList<String>>()

        for (spec in graphSpecs) {
            val labels = spec.split(" <- ")
            labels.zipWithNext().forEach { (prevLabel, label) ->

                if (!targetHashMap.containsKey(prevLabel)) {
                    targetHashMap[prevLabel] = TargetHash("", prevLabel, prevLabel)
                }

                if (!targetHashMap.containsKey(label)) {
                    targetHashMap[label] = TargetHash("", label, label)
                }

                depEdges.computeIfAbsent(label) { mutableListOf() }.add(prevLabel)
            }
        }

        return depEdges to targetHashMap
    }

    fun makeDirectlyChanged(targetHashes: MutableMap<String, TargetHash>, vararg labels: String) {
        for (label in labels) {
            if (!targetHashes.containsKey(label)) {
                throw IllegalArgumentException("Label $label not found in target hashes")
            }
            val orig = targetHashes[label]!!
            targetHashes[label] = orig.copy(
                directHash = orig.directHash + "-changed",
                hash = orig.hash + "-changed"
            )
        }
    }

    fun makeIndirectlyChanged(targetHashes: MutableMap<String, TargetHash>, vararg labels: String) {
        for (label in labels) {
            if (!targetHashes.containsKey(label)) {
                throw IllegalArgumentException("Label $label not found in target hashes")
            }
            val orig = targetHashes[label]!!
            targetHashes[label] = orig.copy(hash = orig.hash + "-changed")
        }
    }
}
