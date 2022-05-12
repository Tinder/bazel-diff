package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.isEqualTo
import com.bazel_diff.testModule
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.mockito.junit.MockitoJUnit

class DeserialiseHashesInteractorTest : KoinTest {
    @get:Rule
    val mockitoRule = MockitoJUnit.rule()

    @get:Rule
    val koinTestRule = KoinTestRule.create {
        modules(testModule())
    }

    @get:Rule
    val temp: TemporaryFolder = TemporaryFolder()

    @Test
    fun testDeserialisation() {
        val file = temp.newFile().apply {
            writeText("{\"target-name\":\"hash\"}")
        }

        val actual = DeserialiseHashesInteractor().execute(file)
        assertThat(actual).isEqualTo(mapOf(
            "target-name" to "hash"
        ))
    }
}
