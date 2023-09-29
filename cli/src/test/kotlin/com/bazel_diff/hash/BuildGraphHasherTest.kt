package com.bazel_diff.hash

import assertk.all
import assertk.assertThat
import assertk.assertions.*
import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelTarget
import com.bazel_diff.log.Logger
import com.bazel_diff.testModule
import kotlinx.coroutines.runBlocking
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.koin.test.mock.MockProviderRule
import org.koin.test.mock.declareMock
import org.mockito.Mockito
import org.mockito.junit.MockitoJUnit
import org.mockito.kotlin.mock
import org.mockito.kotlin.whenever


class BuildGraphHasherTest : KoinTest {
    @get:Rule
    val mockitoRule = MockitoJUnit.rule()

    @get:Rule
    val koinTestRule = KoinTestRule.create {
        modules(testModule())
    }

    @get:Rule
    val mockProvider = MockProviderRule.create { clazz ->
        Mockito.mock(clazz.java)
    }

    @get:Rule
    val temp: TemporaryFolder = TemporaryFolder()

    val bazelClientMock: BazelClient = mock()
    val hasher = BuildGraphHasher(bazelClientMock)


    var defaultTargets: MutableList<BazelTarget> = mutableListOf()

    @Before
    fun setUp() {
        defaultTargets = ArrayList<BazelTarget>().apply {
            add(createRuleTarget(name = "rule1", inputs = ArrayList(), digest = "rule1Digest"))
            add(createRuleTarget(name = "rule2", inputs = ArrayList(), digest = "rule2Digest"))
        }
        Mockito.reset(bazelClientMock)
    }

    @Test
    fun testEmptyBuildGraph() = runBlocking {
        declareMock<Logger>()
        whenever(bazelClientMock.queryAllTargets()).thenReturn(emptyList())
        whenever(bazelClientMock.queryAllSourcefileTargets()).thenReturn(emptyList())

        val hash = hasher.hashAllBazelTargetsAndSourcefiles()
        assertThat(hash).isEmpty()
    }

    @Test
    fun testRuleTargets() = runBlocking {
        declareMock<Logger>()
        whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
        whenever(bazelClientMock.queryAllSourcefileTargets()).thenReturn(emptyList())

        val hash = hasher.hashAllBazelTargetsAndSourcefiles()
        assertThat(hash).containsOnly(
            "rule1" to TargetHash("Rule", "2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775"),
            "rule2" to TargetHash("Rule", "bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9"),
        )
    }

    @Test
    fun testSeedFilepaths() = runBlocking {
        val seedfile = temp.newFile().apply { writeText("somecontent") }.toPath()
        val seedFilepaths = setOf(seedfile)
        whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
        whenever(bazelClientMock.queryAllSourcefileTargets()).thenReturn(emptyList())
        val hash = hasher.hashAllBazelTargetsAndSourcefiles(seedFilepaths)
        assertThat(hash).containsOnly(
            "rule1" to TargetHash("Rule", "0404d80eadcc2dbfe9f0d7935086e1115344a06bd76d4e16af0dfd7b4913ee60"),
            "rule2" to TargetHash("Rule", "6fe63fa16340d18176e6d6021972c65413441b72135247179362763508ebddfe"),
        )
    }

    @Test
    fun hashAllBazelTargets_ruleTargets_ruleInputs() = runBlocking {
        val inputs = listOf("rule1")
        val rule3 = createRuleTarget("rule3", inputs, "digest")
        val rule4 = createRuleTarget("rule4", inputs, "digest2")
        defaultTargets.add(rule3)
        defaultTargets.add(rule4)

        whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
        whenever(bazelClientMock.queryAllSourcefileTargets()).thenReturn(emptyList())
        val hash = hasher.hashAllBazelTargetsAndSourcefiles()
        assertThat(hash).containsOnly(
            "rule1" to TargetHash("Rule", "2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775"),
            "rule2" to TargetHash("Rule", "bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9"),
            "rule3" to TargetHash("Rule", "87dd050f1ca0f684f37970092ff6a02677d995718b5a05461706c0f41ffd4915"),
            "rule4" to TargetHash("Rule", "a7bc5d23cd98c4942dc879c649eb9646e38eddd773f9c7996fa0d96048cf63dc"),
        )
    }

    @Test
    fun testCyclicRuleInput() = runBlocking {
        val ruleInputs = listOf("rule1", "rule4")
        val rule3 = createRuleTarget("rule3", ruleInputs, "digest")
        val rule4 = createRuleTarget("rule4", ruleInputs, "digest2")
        defaultTargets.add(rule3)
        defaultTargets.add(rule4)

        whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
        whenever(bazelClientMock.queryAllSourcefileTargets()).thenReturn(emptyList())
        val hash = hasher.hashAllBazelTargetsAndSourcefiles()
        assertThat(hash).containsOnly(
            "rule1" to TargetHash("Rule", "2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775"),
            "rule2" to TargetHash("Rule", "bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9"),
            "rule3" to TargetHash("Rule", "ca2f970a5a5a18730d7633cc32b48b1d94679f4ccaea56c4924e1f9913bd9cb5"),
            "rule4" to TargetHash("Rule", "bf15e616e870aaacb02493ea0b8e90c6c750c266fa26375e22b30b78954ee523"),
        )
    }

    @Test
    fun testCircularDependency() = runBlocking {
        val rule3 = createRuleTarget("rule3", listOf("rule2", "rule4"), "digest3")
        val rule4 = createRuleTarget("rule4", listOf("rule1", "rule3"), "digest4")
        defaultTargets.add(rule3)
        defaultTargets.add(rule4)
        whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
        whenever(bazelClientMock.queryAllSourcefileTargets()).thenReturn(emptyList())
        assertThat {
            hasher.hashAllBazelTargetsAndSourcefiles()
        }.isFailure().all {
            isInstanceOf(RuleHasher.CircularDependencyException::class)
            // they are run in parallel, so we don't know whether rule3 or rule4 will be processed first
            message().matchesPredicate {
                it!!.contains("\\brule3 -> rule4 -> rule3\\b".toRegex()) ||
                    it.contains("\\brule4 -> rule3 -> rule4\\b".toRegex())
            }
        }
    }

    @Test
    fun testGeneratedTargets() = runBlocking {
        val generator = createRuleTarget("rule1", emptyList(), "rule1Digest")
        val target = createGeneratedTarget("rule0", "rule1")
        val ruleInputs = listOf("rule0")
        val rule3 = createRuleTarget("rule3", ruleInputs, "digest")
        whenever(bazelClientMock.queryAllTargets()).thenReturn(listOf(rule3, target, generator))
        whenever(bazelClientMock.queryAllSourcefileTargets()).thenReturn(emptyList())
        var hash = hasher.hashAllBazelTargetsAndSourcefiles()

        assertThat(hash).hasSize(3)
        val oldHash = hash["rule3"]!!

        whenever(generator.rule.digest(emptySet())).thenReturn("newDigest".toByteArray())
        hash = hasher.hashAllBazelTargetsAndSourcefiles()
        assertThat(hash).hasSize(3)
        val newHash = hash["rule3"]!!
        assertThat(newHash).isNotEqualTo(oldHash)
    }


    private fun createRuleTarget(name: String, inputs: List<String>, digest: String): BazelTarget.Rule {
        val target = mock<BazelTarget.Rule>()
        val rule = mock<BazelRule>()
        whenever(rule.name).thenReturn(name)
        whenever(rule.ruleInputList(false, emptySet())).thenReturn(inputs)
        whenever(rule.digest(emptySet())).thenReturn(digest.toByteArray())
        whenever(target.rule).thenReturn(rule)
        whenever(target.name).thenReturn(name)
        return target
    }

    private fun createGeneratedTarget(name: String, generatingRuleName: String): BazelTarget {
        val target = mock<BazelTarget.GeneratedFile>()
        whenever(target.name).thenReturn(name)
        whenever(target.generatedFileName).thenReturn(name)
        whenever(target.generatingRuleName).thenReturn(generatingRuleName)
        return target
    }
}
