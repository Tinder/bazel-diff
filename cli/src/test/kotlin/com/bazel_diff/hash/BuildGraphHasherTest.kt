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
        val mod = module { single<SourceFileHasher> { fakeSourceFileHasher } }
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
                    "7b3149cbd2219ca05bc80a557a701ddee18bd3bbe9afa8e851df64b999155c5e",
                    "2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775",
                    emptyList()),
            "rule2" to
                TargetHash(
                    "Rule",
                    "24f12d22ab247c5af32f954ca46dd4f6323ab2eef28455411b912aaf44a7c322",
                    "bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9",
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
                    "7b3149cbd2219ca05bc80a557a701ddee18bd3bbe9afa8e851df64b999155c5e",
                    "2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775",
                    emptyList()),
            "rule2" to
                TargetHash(
                    "Rule",
                    "24f12d22ab247c5af32f954ca46dd4f6323ab2eef28455411b912aaf44a7c322",
                    "bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9",
                    emptyList()),
            "rule3" to
                TargetHash(
                    "Rule",
                    "c7018bbfed16f4f6f0ef1f258024a50c56ba916b3b9ed4f00972a233d5d11b42",
                    "4aeafed087a9c78a4efa11b6f7831c38d775ddb244a9fabbf21d78c1666a2ea9",
                    listOf("rule1")),
            "rule4" to
                TargetHash(
                    "Rule",
                    "020720dfbb969ef9629e51a624a616f015fe07c7b779a5b4f82a8b36c9d3cbe9",
                    "82b46404c8a1ec402a60de72d42a14f6a080e938e5ebaf26203c5ef480558767",
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
                    "7b3149cbd2219ca05bc80a557a701ddee18bd3bbe9afa8e851df64b999155c5e",
                    "2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775",
                    emptyList()),
            "rule2" to
                TargetHash(
                    "Rule",
                    "24f12d22ab247c5af32f954ca46dd4f6323ab2eef28455411b912aaf44a7c322",
                    "bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9",
                    emptyList()),
            "rule3" to
                TargetHash(
                    "Rule",
                    "be17f1e1884037b970e6b7c86bb6533b253a12d967029adc711e50d4662237e8",
                    "91ea3015d4424bb8c1ecf381c30166c386c161d31b70967f3a021c1dc43c7774",
                    listOf("rule1", "rule4")),
            "rule4" to
                TargetHash(
                    "Rule",
                    "f3e5675e30fe25ff9b61a0c7f64c423964f886799407a9438e692fd803ecd47c",
                    "bce09e1689cc7a8172653981582fea70954f8acd58985c92026582e4b75ec8d2",
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
