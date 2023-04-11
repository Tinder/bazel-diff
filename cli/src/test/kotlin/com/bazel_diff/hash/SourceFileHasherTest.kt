package com.bazel_diff.hash

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isNull
import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.testModule
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import java.nio.file.Files
import java.nio.file.Paths


internal class SourceFileHasherTest: KoinTest {
    private val repoAbsolutePath = Paths.get("").toAbsolutePath()
    private val outputBasePath = Files.createTempDirectory("SourceFileHasherTest")
    private val fixtureFileTarget = "//cli/src/test/kotlin/com/bazel_diff/hash/fixture:foo.ts"
    private val fixtureFileContent: ByteArray
    private val seed = "seed".toByteArray()

    init {
        val path = Paths.get("cli/src/test/kotlin/com/bazel_diff/hash/fixture/foo.ts")
        fixtureFileContent = Files.readAllBytes(path)
    }


    @get:Rule
    val koinTestRule = KoinTestRule.create {
        modules(testModule())
    }

    @Test
    fun testHashConcreteFile() = runBlocking {
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, null)
        val bazelSourceFileTarget = BazelSourceFileTarget(fixtureFileTarget, seed)
        val actual = hasher.digest(bazelSourceFileTarget).toHexString()
        val expected = sha256 {
            safePutBytes(fixtureFileContent)
            safePutBytes(seed)
            safePutBytes(fixtureFileTarget.toByteArray())
        }.toHexString()
        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testHashConcreteFileInExternalRepo() = runBlocking {
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, null, setOf("external_repo"))
        val externalRepoFilePath = outputBasePath.resolve("external/external_repo/path/to/my_file.txt")
        Files.createDirectories(externalRepoFilePath.parent)
        val externalRepoFileTarget = "@external_repo//path/to:my_file.txt"
        val externalRepoFileContent = "hello world"
        externalRepoFilePath.toFile().writeText(externalRepoFileContent)
        val bazelSourceFileTarget = BazelSourceFileTarget(externalRepoFileTarget, seed)
        val actual = hasher.digest(bazelSourceFileTarget).toHexString()
        val expected = sha256 {
            safePutBytes(externalRepoFileContent.toByteArray())
            safePutBytes(seed)
            safePutBytes(externalRepoFileTarget.toByteArray())
        }.toHexString()
        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testSoftHashConcreteFile() = runBlocking {
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, null)
        val bazelSourceFileTarget = BazelSourceFileTarget(fixtureFileTarget, seed)
        val actual = hasher.softDigest(bazelSourceFileTarget)?.toHexString()
        val expected = sha256 {
            safePutBytes(fixtureFileContent)
            safePutBytes(seed)
            safePutBytes(fixtureFileTarget.toByteArray())
        }.toHexString()
        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testSoftHashNonExistedFile() = runBlocking {
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, null)
        val bazelSourceFileTarget = BazelSourceFileTarget("//i/do/not/exist", seed)
        val actual = hasher.softDigest(bazelSourceFileTarget)
        assertThat(actual).isNull()
    }

    @Test
    fun testSoftHashExternalTarget() = runBlocking {
        val target = "@bazel-diff//some:file"
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, null)
        val bazelSourceFileTarget = BazelSourceFileTarget(target, seed)
        val actual = hasher.softDigest(bazelSourceFileTarget)
        assertThat(actual).isNull()
    }

    @Test
    fun testHashNonExistedFile() = runBlocking {
        val target = "//i/do/not/exist"
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, null)
        val bazelSourceFileTarget = BazelSourceFileTarget(target, seed)
        val actual = hasher.digest(bazelSourceFileTarget).toHexString()
        val expected = sha256 {
            safePutBytes(seed)
            safePutBytes(target.toByteArray())
        }.toHexString()
        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testHashExternalTarget() = runBlocking {
        val target = "@bazel-diff//some:file"
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, null)
        val bazelSourceFileTarget = BazelSourceFileTarget(target, seed)
        val actual = hasher.digest(bazelSourceFileTarget).toHexString()
        val expected = sha256 {}.toHexString()
        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testHashWithProvidedContentHash() = runBlocking {
        val filenameToContentHash = hashMapOf("cli/src/test/kotlin/com/bazel_diff/hash/fixture/foo.ts" to "foo-content-hash")
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, filenameToContentHash)
        val bazelSourceFileTarget = BazelSourceFileTarget(fixtureFileTarget, seed)
        val actual = hasher.digest(bazelSourceFileTarget).toHexString()
        val expected = sha256 {
            safePutBytes("foo-content-hash".toByteArray())
            safePutBytes(seed)
            safePutBytes(fixtureFileTarget.toByteArray())
        }.toHexString()
        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testHashWithProvidedContentHashButNotInKey() = runBlocking {
        val filenameToContentHash = hashMapOf("cli/src/test/kotlin/com/bazel_diff/hash/fixture/bar.ts" to "foo-content-hash")
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, filenameToContentHash)
        val bazelSourceFileTarget = BazelSourceFileTarget(fixtureFileTarget, seed)
        val actual = hasher.digest(bazelSourceFileTarget).toHexString()
        val expected = sha256 {
            safePutBytes(fixtureFileContent)
            safePutBytes(seed)
            safePutBytes(fixtureFileTarget.toByteArray())
        }.toHexString()
        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testHashWithProvidedContentHashWithLeadingColon() = runBlocking {
        val targetName = "//:cli/src/test/kotlin/com/bazel_diff/hash/fixture/bar.ts"
        val filenameToContentHash = hashMapOf("cli/src/test/kotlin/com/bazel_diff/hash/fixture/bar.ts" to "foo-content-hash")
        val hasher = SourceFileHasher(repoAbsolutePath, outputBasePath, filenameToContentHash)
        val bazelSourceFileTarget = BazelSourceFileTarget(targetName, seed)
        val actual = hasher.digest(bazelSourceFileTarget).toHexString()
        val expected = sha256 {
            safePutBytes("foo-content-hash".toByteArray())
            safePutBytes(seed)
            safePutBytes(targetName.toByteArray())
        }.toHexString()
        assertThat(actual).isEqualTo(expected)
    }
}
