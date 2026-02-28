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
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.koin.test.mock.MockProviderRule
import org.koin.test.mock.declareMock
import org.mockito.Mockito
import org.mockito.junit.MockitoJUnit
import org.mockito.kotlin.mock
import org.mockito.kotlin.whenever

class BuildGraphHasherTest : KoinTest {
  @get:Rule val mockitoRule = MockitoJUnit.rule()

  @get:Rule
  val koinTestRule =
      KoinTestRule.create {
        val mod = module {
          single<SourceFileHasher> { fakeSourceFileHasher }
          single<com.bazel_diff.bazel.BazelModService> {
            mock<com.bazel_diff.bazel.BazelModService>().apply {
              runBlocking {
                whenever(getModuleGraph()).thenReturn(null)
              }
            }
          }
        }
        modules(testModule(), mod)
      }

  @get:Rule val mockProvider = MockProviderRule.create { clazz -> Mockito.mock(clazz.java) }

  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  val bazelClientMock: BazelClient = mock()
  val hasher = BuildGraphHasher(bazelClientMock)

  var defaultTargets: MutableList<BazelTarget> = mutableListOf()

  var fakeSourceFileHasher: FakeSourceFileHasher = FakeSourceFileHasher()

  @Before
  fun setUp() {
    defaultTargets =
        ArrayList<BazelTarget>().apply {
          add(createRuleTarget(name = "rule1", inputs = ArrayList(), digest = "rule1Digest"))
          add(createRuleTarget(name = "rule2", inputs = ArrayList(), digest = "rule2Digest"))
        }
    Mockito.reset(bazelClientMock)
  }

  @Test
  fun testEmptyBuildGraph() = runBlocking {
    declareMock<Logger>()
    whenever(bazelClientMock.queryAllTargets()).thenReturn(emptyList())

    val hash = hasher.hashAllBazelTargetsAndSourcefiles()
    assertThat(hash).isEmpty()
  }

  @Test
  fun testRuleTargets() = runBlocking {
    declareMock<Logger>()
    whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)

    val hash = hasher.hashAllBazelTargetsAndSourcefiles()
    assertThat(hash)
        .containsOnly(
            "rule1" to
                TargetHash(
                    "Rule",
                    "443e5e8ca6f5f1afdebfb8650a1fc37ad10964fca5a270ef97b37ea2425c57ee",
                    "e34f24077b917a720bc1ce4b1383c7df3c01ff26e492dea43d45780871b30382",
                    emptyList()),
            "rule2" to
                TargetHash(
                    "Rule",
                    "72697540fd5f7d4fd23832a046915630cb2658f8cd0e67bc9c98353131bebfbc",
                    "9cfe592887d063932fa455fe985e96d8ec07079bc20fc2a955e8e8d520c59282",
                    emptyList()),
        )
  }

  @Test
  fun testSeedFilepaths() = runBlocking {
    val seedfile = temp.newFile().apply { writeText("somecontent") }.toPath()
    val seedFilepaths = setOf(seedfile)
    whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
    val hash = hasher.hashAllBazelTargetsAndSourcefiles(seedFilepaths)
    assertThat(hash)
        .containsOnly(
            "rule1" to
                TargetHash(
                    "Rule",
                    "ddf7345122667dda1bbbdc813a6d029795b243e729bcfdcd520cde12cac877f1",
                    "0404d80eadcc2dbfe9f0d7935086e1115344a06bd76d4e16af0dfd7b4913ee60",
                    emptyList()),
            "rule2" to
                TargetHash(
                    "Rule",
                    "41cbcdcbc90aafbeb74b903e7cadcf0c16f36f28dbd7f4cb72699449ff1ab11d",
                    "6fe63fa16340d18176e6d6021972c65413441b72135247179362763508ebddfe",
                    emptyList()),
        )
  }

  @Test
  fun hashAllBazelTargets_directHashUnchangedByDeps() = runBlocking {
    val rule1 = createRuleTarget(name = "rule1", inputs = ArrayList(), digest = "rule1Digest")
    val rule2 =
        createRuleTarget(name = "rule2", inputs = arrayListOf("rule1"), digest = "rule2Digest")
    val rule3 =
        createRuleTarget(name = "rule3", inputs = arrayListOf("rule2"), digest = "rule3Digest")
    val targets = arrayListOf(rule1, rule2, rule3)

    whenever(bazelClientMock.queryAllTargets()).thenReturn(targets)
    val baseHashes = hasher.hashAllBazelTargetsAndSourcefiles()

    val newRule1 =
        createRuleTarget(name = "rule1", inputs = ArrayList(), digest = "rule1Digest--changed")
    val newTargets = arrayListOf(newRule1, rule2, rule3)

    whenever(bazelClientMock.queryAllTargets()).thenReturn(newTargets)
    val newHashes = hasher.hashAllBazelTargetsAndSourcefiles()

    val hashes = HashDiffer(baseHashes, newHashes)

    hashes.assertThat("rule1").hash().changed()
    hashes.assertThat("rule2").hash().changed()
    hashes.assertThat("rule3").hash().changed()

    hashes.assertThat("rule1").directHash().changed()
    hashes.assertThat("rule2").directHash().didNotChange()
    hashes.assertThat("rule3").directHash().didNotChange()
  }

  @Test
  fun hashAllBazelTargets_directHashChangedBySrcFiles() = runBlocking {
    val src1 = createSrcTarget(name = "src1", digest = "src1")
    val src2 = createSrcTarget(name = "src2", digest = "src2")
    val src3 = createSrcTarget(name = "src3", digest = "src3")
    val rule1 =
        createRuleTarget(
            name = "rule1", inputs = arrayListOf("src1", "src2"), digest = "rule1Digest")
    val rule2 =
        createRuleTarget(
            name = "rule2", inputs = arrayListOf("src3", "rule1"), digest = "rule2Digest")
    val targets = arrayListOf(src1, src2, src3, rule1, rule2)

    whenever(bazelClientMock.queryAllTargets()).thenReturn(targets)
    val baseHashes = hasher.hashAllBazelTargetsAndSourcefiles()

    val newSrc1 = createSrcTarget(name = "src1", digest = "src1--modified")
    val newTargets = arrayListOf(newSrc1, src2, src3, rule1, rule2)

    whenever(bazelClientMock.queryAllTargets()).thenReturn(newTargets)
    val newHashes = hasher.hashAllBazelTargetsAndSourcefiles()

    val hashes = HashDiffer(baseHashes, newHashes)

    hashes.assertThat("rule1").hash().changed()
    hashes.assertThat("rule1").directHash().changed()

    hashes.assertThat("rule2").hash().changed()
    hashes.assertThat("rule2").directHash().didNotChange()
  }

  @Test
  fun hashAllBazelTargets_generatedSrcDoesNotContributeToDirect() = runBlocking {
    val rule1 = createRuleTarget(name = "rule1", inputs = emptyList(), digest = "rule1Digest")
    val src1 = createGeneratedTarget("gen1", "rule1")
    val rule2 =
        createRuleTarget(name = "rule2", inputs = arrayListOf("gen1"), digest = "rule2Digest")
    val targets = arrayListOf(rule1, src1, rule2)

    whenever(bazelClientMock.queryAllTargets()).thenReturn(targets)
    val baseHashes = hasher.hashAllBazelTargetsAndSourcefiles()

    val newRule1 =
        createRuleTarget(name = "rule1", inputs = emptyList(), digest = "rule1Digest--changed")
    val newTargets = arrayListOf(newRule1, src1, rule2)

    whenever(bazelClientMock.queryAllTargets()).thenReturn(newTargets)
    val newHashes = hasher.hashAllBazelTargetsAndSourcefiles()

    val hashes = HashDiffer(baseHashes, newHashes)

    hashes.assertThat("rule1").hash().changed()
    hashes.assertThat("rule1").directHash().changed()

    hashes.assertThat("gen1").hash().changed()
    hashes.assertThat("gen1").directHash().changed()

    hashes.assertThat("rule2").hash().changed()
    hashes.assertThat("rule2").directHash().didNotChange()
  }

  @Test
  fun hashAllBazelTargets_ruleTargets_ruleInputs() = runBlocking {
    val inputs = listOf("rule1")
    val rule3 = createRuleTarget("rule3", inputs, "digest")
    val rule4 = createRuleTarget("rule4", inputs, "digest2")
    defaultTargets.add(rule3)
    defaultTargets.add(rule4)

    whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
    val hash = hasher.hashAllBazelTargetsAndSourcefiles()
    assertThat(hash)
        .containsOnly(
            "rule1" to
                TargetHash(
                    "Rule",
                    "443e5e8ca6f5f1afdebfb8650a1fc37ad10964fca5a270ef97b37ea2425c57ee",
                    "e34f24077b917a720bc1ce4b1383c7df3c01ff26e492dea43d45780871b30382",
                    emptyList()),
            "rule2" to
                TargetHash(
                    "Rule",
                    "72697540fd5f7d4fd23832a046915630cb2658f8cd0e67bc9c98353131bebfbc",
                    "9cfe592887d063932fa455fe985e96d8ec07079bc20fc2a955e8e8d520c59282",
                    emptyList()),
            "rule3" to
                TargetHash(
                    "Rule",
                    "504431ed5d613c0a165e0be7f492d82f03b9f48cc58fc284303967fc1946f9f5",
                    "a572cd448866842478ead3689e13849a6069e07e50c79dc9b2be0bd282812eba",
                    listOf("rule1")),
            "rule4" to
                TargetHash(
                    "Rule",
                    "7ce78e40ee07a1fa9f2cbcfc3ecf65295e0455b0dcee03b66fd1a21822e39c94",
                    "c2743eef104170464c7bb67d7c399070e0d68ae3efb5564da796cc50218a4a48",
                    listOf("rule1")),
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
    val hash = hasher.hashAllBazelTargetsAndSourcefiles()
    assertThat(hash)
        .containsOnly(
            "rule1" to
                TargetHash(
                    "Rule",
                    "443e5e8ca6f5f1afdebfb8650a1fc37ad10964fca5a270ef97b37ea2425c57ee",
                    "e34f24077b917a720bc1ce4b1383c7df3c01ff26e492dea43d45780871b30382",
                    emptyList()),
            "rule2" to
                TargetHash(
                    "Rule",
                    "72697540fd5f7d4fd23832a046915630cb2658f8cd0e67bc9c98353131bebfbc",
                    "9cfe592887d063932fa455fe985e96d8ec07079bc20fc2a955e8e8d520c59282",
                    emptyList()),
            "rule3" to
                TargetHash(
                    "Rule",
                    "2c424742eaeedfecd438042735f97922e87d73aaabfa28bb2e7a97665f3cb4df",
                    "a34330110423a54360c53c8dfa6181fa7ff484b55795a41aa471c77e472d8895",
                    listOf("rule1", "rule4")),
            "rule4" to
                TargetHash(
                    "Rule",
                    "55af9fde90871f0af4bc588cab392d89c2e58220b3e1fc2c74a55857a3842b89",
                    "8ab85c9e6eb4c2320396c682106bf712535ac7e4c7204b7bcda2dacad7167d3b",
                    listOf("rule1")),
        )
  }

  @Test
  fun testCircularDependency() = runBlocking {
    val rule3 = createRuleTarget("rule3", listOf("rule2", "rule4"), "digest3")
    val rule4 = createRuleTarget("rule4", listOf("rule1", "rule3"), "digest4")
    defaultTargets.add(rule3)
    defaultTargets.add(rule4)
    whenever(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets)
    assertThat { hasher.hashAllBazelTargetsAndSourcefiles() }
        .isFailure()
        .all {
          isInstanceOf(RuleHasher.CircularDependencyException::class)
          // they are run in parallel, so we don't know whether rule3 or rule4 will be processed
          // first
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
    var hash = hasher.hashAllBazelTargetsAndSourcefiles()

    assertThat(hash).hasSize(3)
    val oldHash = hash["rule3"]!!

    whenever(generator.rule.digest(emptySet())).thenReturn("newDigest".toByteArray())
    hash = hasher.hashAllBazelTargetsAndSourcefiles()
    assertThat(hash).hasSize(3)
    val newHash = hash["rule3"]!!
    assertThat(newHash).isNotEqualTo(oldHash)
  }

  @Test
  fun testGeneratedTargetsDeps() = runBlocking {
    // GeneratedSrcs do not have RuleInputs in the bazel query proto,
    // so ensure that we are properly tracking their dependency edges to
    // the targets that generate them.
    val generator = createRuleTarget("rule1", emptyList(), "rule1Digest")
    val target = createGeneratedTarget("rule0", "rule1")
    val ruleInputs = listOf("rule0")
    val rule3 = createRuleTarget("rule3", ruleInputs, "digest")

    whenever(bazelClientMock.queryAllTargets()).thenReturn(listOf(rule3, target, generator))
    var hash = hasher.hashAllBazelTargetsAndSourcefiles()

    val depsMapping = hash.mapValues { it.value.deps }

    assertThat(depsMapping)
        .containsOnly(
            "rule3" to listOf("rule0"), "rule0" to listOf("rule1"), "rule1" to emptyList<String>())
  }

  private fun createRuleTarget(
      name: String,
      inputs: List<String>,
      digest: String
  ): BazelTarget.Rule {
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

  private fun createSrcTarget(name: String, digest: String): BazelTarget {
    fakeSourceFileHasher.add(name, digest.toByteArray())

    val target = mock<BazelTarget.SourceFile>()
    whenever(target.name).thenReturn(name)
    whenever(target.sourceFileName).thenReturn(name)
    whenever(target.subincludeList).thenReturn(listOf())
    return target
  }
}
