package com.bazel_diff.io

import assertk.assertThat
import assertk.assertions.*
import com.bazel_diff.testModule
import com.google.gson.JsonSyntaxException
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import java.io.File

internal class ContentHashProviderTest: KoinTest {
    @get:Rule
    val koinTestRule = KoinTestRule.create {
        modules(testModule())
    }

    @Test
    fun testNullPath() = runBlocking {
        val contentHashProvider = ContentHashProvider(null)
        assertThat(contentHashProvider.filenameToHash).isNull()
    }

    @Test
    fun testNonExistingPath() = runBlocking {
        assertThat {
            ContentHashProvider(File("/not/exists"))
        }.isFailure().hasClass(java.io.FileNotFoundException::class)
    }

    @Test
    fun testParseJsonFileWithWrongShape() = runBlocking {
        val file = File("cli/src/test/kotlin/com/bazel_diff/io/fixture/wrong.json")
        assertThat {
            val a = ContentHashProvider(file)
            println(a.filenameToHash)
        }.isFailure().hasClass(JsonSyntaxException::class)
    }

    @Test
    fun testParseJsonFileWithCorrectShape() = runBlocking {
        val file = File("cli/src/test/kotlin/com/bazel_diff/io/fixture/correct.json")
        val map = ContentHashProvider(file).filenameToHash
        assertThat(map).isNotNull().containsOnly(
            "foo" to "content-hash-for-foo",
            "bar" to "content-hash-for-bar"
        )
    }
}
