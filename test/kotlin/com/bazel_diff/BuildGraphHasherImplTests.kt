package com.bazel_diff

import org.junit.Assert
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.mockito.ArgumentMatchers
import org.mockito.Mock
import org.mockito.Mockito
import org.mockito.junit.MockitoJUnit
import java.io.IOException
import java.nio.file.Path
import java.nio.file.Paths
import java.security.NoSuchAlgorithmException
import java.util.*

class BuildGraphHasherImplTests {
    @Mock
    var bazelClientMock: BazelClient? = null

    @Mock
    var filesClientMock: FilesClient? = null
    var defaultTargets: MutableList<BazelTarget>? = null

    @Rule
    var mockitoRule = MockitoJUnit.rule()
    @Before
    @Throws(Exception::class)
    fun setUp() {
        defaultTargets = ArrayList()
        defaultTargets.add(createRuleTarget("rule1", ArrayList(), "rule1Digest"))
        defaultTargets.add(createRuleTarget("rule2", ArrayList(), "rule2Digest"))
    }

    @Test
    @Throws(IOException::class)
    fun hashAllBazelTargets_noTargets() {
        Mockito.`when`(bazelClientMock!!.queryAllTargets()).thenReturn(ArrayList())
        val client = com.bazel_diff.hash.DefaultTargetHashingClient(bazelClientMock!!, filesClientMock!!)
        try {
            val hash = client.hashAllBazelTargetsAndSourcefiles(HashSet())
            Assert.assertEquals(hash.size.toLong(), 0)
        } catch (e: Exception) {
            Assert.fail(e.message)
        }
    }

    @Test
    @Throws(IOException::class)
    fun hashAllBazelTargets_ruleTargets() {
        Mockito.`when`(bazelClientMock!!.queryAllTargets()).thenReturn(defaultTargets)
        val client = com.bazel_diff.hash.DefaultTargetHashingClient(bazelClientMock!!, filesClientMock!!)
        try {
            val hash = client.hashAllBazelTargetsAndSourcefiles(HashSet())
            Assert.assertEquals(2, hash.size.toLong())
            Assert.assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash["rule1"])
            Assert.assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash["rule2"])
        } catch (e: Exception) {
            Assert.fail(e.message)
        }
    }

    @Test
    @Throws(IOException::class)
    fun hashAllBazelTargets_ruleTargets_seedFilepaths() {
        val seedFilepaths: MutableSet<Path> = HashSet()
        seedFilepaths.add(Paths.get("somefile.txt"))
        Mockito.`when`(filesClientMock!!.readFile(ArgumentMatchers.anyObject())).thenReturn("somecontent".toByteArray())
        Mockito.`when`(bazelClientMock!!.queryAllTargets()).thenReturn(defaultTargets)
        val client = com.bazel_diff.hash.DefaultTargetHashingClient(bazelClientMock!!, filesClientMock!!)
        try {
            val hash = client.hashAllBazelTargetsAndSourcefiles(seedFilepaths)
            Assert.assertEquals(2, hash.size.toLong())
            Assert.assertEquals("0404d80eadcc2dbfe9f0d7935086e1115344a06bd76d4e16af0dfd7b4913ee60", hash["rule1"])
            Assert.assertEquals("6fe63fa16340d18176e6d6021972c65413441b72135247179362763508ebddfe", hash["rule2"])
        } catch (e: Exception) {
            Assert.fail(e.message)
        }
    }

    @Test
    @Throws(IOException::class, NoSuchAlgorithmException::class)
    fun hashAllBazelTargets_ruleTargets_ruleInputs() {
        val ruleInputs: MutableList<String> = ArrayList()
        ruleInputs.add("rule1")
        val rule3 = createRuleTarget("rule3", ruleInputs, "digest")
        defaultTargets!!.add(rule3)
        val rule4 = createRuleTarget("rule4", ruleInputs, "digest2")
        defaultTargets!!.add(rule4)
        Mockito.`when`(bazelClientMock!!.queryAllTargets()).thenReturn(defaultTargets)
        val client = com.bazel_diff.hash.DefaultTargetHashingClient(bazelClientMock!!, filesClientMock!!)
        try {
            val hash = client.hashAllBazelTargetsAndSourcefiles(HashSet())
            Assert.assertEquals(4, hash.size.toLong())
            Assert.assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash["rule1"])
            Assert.assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash["rule2"])
            Assert.assertEquals("87dd050f1ca0f684f37970092ff6a02677d995718b5a05461706c0f41ffd4915", hash["rule3"])
            Assert.assertEquals("a7bc5d23cd98c4942dc879c649eb9646e38eddd773f9c7996fa0d96048cf63dc", hash["rule4"])
        } catch (e: Exception) {
            Assert.fail(e.message)
        }
    }

    @Test
    @Throws(IOException::class, NoSuchAlgorithmException::class)
    fun hashAllBazelTargets_ruleTargets_ruleInputsWithSelfInput() {
        val ruleInputs: MutableList<String> = ArrayList()
        ruleInputs.add("rule1")
        ruleInputs.add("rule4")
        val rule3 = createRuleTarget("rule3", ruleInputs, "digest")
        defaultTargets!!.add(rule3)
        val rule4 = createRuleTarget("rule4", ruleInputs, "digest2")
        defaultTargets!!.add(rule4)
        Mockito.`when`(bazelClientMock!!.queryAllTargets()).thenReturn(defaultTargets)
        val client = com.bazel_diff.hash.DefaultTargetHashingClient(bazelClientMock!!, filesClientMock!!)
        try {
            val hash = client.hashAllBazelTargetsAndSourcefiles(HashSet())
            Assert.assertEquals(4, hash.size.toLong())
            Assert.assertEquals("bf15e616e870aaacb02493ea0b8e90c6c750c266fa26375e22b30b78954ee523", hash["rule4"])
        } catch (e: Exception) {
            Assert.fail(e.message)
        }
    }

    @Test
    @Throws(Exception::class)
    fun HashAllBazelTargets_generatedTargets() {
        val generator = createRuleTarget("rule1", ArrayList(), "rule1Digest")
        val target = createGeneratedTarget("rule0", "rule1")
        val ruleInputs: MutableList<String> = ArrayList()
        ruleInputs.add("rule0")
        val rule3 = createRuleTarget("rule3", ruleInputs, "digest")
        var oldHash = ""
        var newHash = ""
        Mockito.`when`(bazelClientMock!!.queryAllTargets()).thenReturn(Arrays.asList(rule3, target, generator))
        val client = com.bazel_diff.hash.DefaultTargetHashingClient(bazelClientMock!!, filesClientMock!!)
        var hash: Map<String?, String?> = client.hashAllBazelTargetsAndSourcefiles(HashSet())
        Assert.assertEquals(3, hash.size.toLong())
        oldHash = hash["rule3"]
        Mockito.`when`(generator.rule.digest).thenReturn("newDigest".toByteArray())
        hash = client.hashAllBazelTargetsAndSourcefiles(HashSet())
        Assert.assertEquals(3, hash.size.toLong())
        newHash = hash["rule3"]
        Assert.assertNotEquals(oldHash, newHash)
    }

    @Test
    fun TestImpactedTargets() {
        val client = com.bazel_diff.hash.DefaultTargetHashingClient(bazelClientMock!!, filesClientMock!!)
        try {
            val start: MutableMap<String, String> = HashMap()
            start["1"] = "a"
            start["2"] = "b"
            val end: MutableMap<String, String> = HashMap()
            end["1"] = "c"
            end["2"] = "b"
            end["3"] = "d"
            val impacted = client.getImpactedTargets(start, end)
            Assert.assertEquals(2, impacted.size.toLong())
            Assert.assertArrayEquals(arrayOf("1", "3"), impacted.toTypedArray())
        } catch (e: Exception) {
            Assert.fail(e.message)
        }
    }

    @Throws(NoSuchAlgorithmException::class)
    private fun createRuleTarget(ruleName: String, ruleInputs: List<String>, ruleDigest: String): BazelTarget {
        val target = Mockito.mock(BazelTarget::class.java)
        val rule = Mockito.mock(BazelRule::class.java)
        Mockito.`when`<String>(rule.getName()).thenReturn(ruleName)
        Mockito.`when`<List<String>>(rule.getRuleInputList()).thenReturn(ruleInputs)
        Mockito.`when`<ByteArray>(rule.getDigest()).thenReturn(ruleDigest.toByteArray())
        Mockito.`when`(target.rule).thenReturn(rule)
        Mockito.`when`(target.hasRule()).thenReturn(true)
        Mockito.`when`(target.hasSourceFile()).thenReturn(false)
        return target
    }

    private fun createSourceTarget(name: String): BazelTarget {
        val target = Mockito.mock(BazelTarget::class.java)
        Mockito.`when`(target.hasSourceFile()).thenReturn(true)
        Mockito.`when`(target.hasRule()).thenReturn(false)
        Mockito.`when`(target.rule).thenReturn(null)
        Mockito.`when`(target.sourceFileName).thenReturn(name)
        return target
    }

    @Throws(NoSuchAlgorithmException::class)
    private fun createSourceFileTarget(name: String, digest: String): BazelSourceFileTarget {
        val target = Mockito.mock(
            BazelSourceFileTarget::class.java
        )
        Mockito.`when`(target.name).thenReturn(name)
        Mockito.`when`(target.digest).thenReturn(digest.toByteArray())
        return target
    }

    @Throws(NoSuchAlgorithmException::class)
    private fun createGeneratedTarget(name: String, generatingRuleName: String): BazelTarget {
        val target = Mockito.mock(BazelTarget::class.java)
        Mockito.`when`(target.hasRule()).thenReturn(false)
        Mockito.`when`(target.hasSourceFile()).thenReturn(false)
        Mockito.`when`(target.hasGeneratedFile()).thenReturn(true)
        Mockito.`when`(target.generatedFileName).thenReturn(name)
        Mockito.`when`(target.generatingRuleName).thenReturn(generatingRuleName)
        return target
    }
}
