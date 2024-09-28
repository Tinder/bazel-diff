package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.containsExactlyInAnyOrder
import com.bazel_diff.testModule
import org.junit.Rule
import org.junit.Test
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.mockito.junit.MockitoJUnit
import com.bazel_diff.hash.TargetHash

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

        val impacted = interactor.execute(startHashes, endHashes)
        assertThat(impacted).containsExactlyInAnyOrder(
            "1", "3"
        )
    }
}
