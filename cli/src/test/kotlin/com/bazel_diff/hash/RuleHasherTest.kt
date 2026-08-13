package com.bazel_diff.hash

import assertk.assertFailure
import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEqualTo
import assertk.assertions.isInstanceOf
import assertk.assertions.isNotEqualTo
import assertk.assertions.isNotNull
import assertk.assertions.isNull
import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.log.Logger
import com.bazel_diff.testModule
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Attribute
import java.util.concurrent.ConcurrentHashMap
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.koin.test.mock.MockProviderRule
import org.koin.test.mock.declareMock
import org.mockito.Mockito
import org.mockito.junit.MockitoJUnit
import org.mockito.kotlin.argumentCaptor
import org.mockito.kotlin.atLeastOnce
import org.mockito.kotlin.verify

/**
 * Unit coverage for [RuleHasher.digest] paths not already pinned by
 * [RuleHasherAlwaysAffectedTagsTest].
 */
class RuleHasherTest : KoinTest {
  @get:Rule val mockitoRule = MockitoJUnit.rule()

  @get:Rule val mockProvider = MockProviderRule.create { clazz -> Mockito.mock(clazz.java) }

  private val fake = FakeSourceFileHasher()

  @get:Rule
  val koinTestRule =
      KoinTestRule.create {
        modules(
            testModule(),
            module { single<SourceFileHasher> { fake } },
        )
      }

  @Before
  fun setUp() {
    fake.softDigestValue = "fake-soft-digest".toByteArray()
    fake.softDigestCalls.set(0)
    fake.fakeDigests.clear()
  }

  private fun rule(
      name: String,
      ruleClass: String = "sh_test",
      ruleInputs: List<String> = emptyList(),
      configuredRuleInputs: List<Pair<String, String>> = emptyList(),
      visibility: List<String> = emptyList(),
      instantiationStack: List<String> = emptyList(),
      tags: List<String> = emptyList(),
  ): BazelRule {
    val builder = Build.Rule.newBuilder().setName(name).setRuleClass(ruleClass)
    ruleInputs.forEach { builder.addRuleInput(it) }
    configuredRuleInputs.forEach { (label, checksum) ->
      builder.addConfiguredRuleInput(
          Build.ConfiguredRuleInput.newBuilder()
              .setLabel(label)
              .setConfigurationChecksum(checksum)
              .build())
    }
    if (visibility.isNotEmpty()) {
      val attr =
          Attribute.newBuilder().setName("visibility").setType(Attribute.Discriminator.STRING_LIST)
      visibility.forEach { attr.addStringListValue(it) }
      builder.addAttribute(attr.build())
    }
    if (tags.isNotEmpty()) {
      val attr = Attribute.newBuilder().setName("tags").setType(Attribute.Discriminator.STRING_LIST)
      tags.forEach { attr.addStringListValue(it) }
      builder.addAttribute(attr.build())
    }
    instantiationStack.forEach { builder.addInstantiationStack(it) }
    return BazelRule(builder.build())
  }

  private fun packageGroup(name: String, packages: List<String> = listOf("//...")): BazelRule {
    val attr =
        Attribute.newBuilder().setName("packages").setType(Attribute.Discriminator.STRING_LIST)
    packages.forEach { attr.addStringListValue(it) }
    return BazelRule(
        Build.Rule.newBuilder()
            .setName(name)
            .setRuleClass("package_group")
            .addAttribute(attr.build())
            .build())
  }

  private fun hasher(
      useCquery: Boolean = false,
      trackDepLabels: Boolean = true,
      alwaysAffectedTags: Set<String> = emptySet(),
      alwaysAffectedSeed: ByteArray = ByteArray(0),
  ): RuleHasher =
      RuleHasher(
          useCquery = useCquery,
          trackDepLabels = trackDepLabels,
          fineGrainedHashExternalRepos = emptySet(),
          alwaysAffectedTags = alwaysAffectedTags,
          alwaysAffectedSeed = alwaysAffectedSeed)

  private fun digest(
      hasher: RuleHasher = hasher(),
      rule: BazelRule,
      allRulesMap: Map<String, BazelRule> = mapOf(rule.name to rule),
      ruleHashes: ConcurrentHashMap<String, TargetDigest> = ConcurrentHashMap(),
      sourceDigests: ConcurrentHashMap<String, ByteArray> = ConcurrentHashMap(),
      seedHash: ByteArray? = ByteArray(0),
      packageBzlSeeds: Map<String, ByteArray> = emptyMap(),
      depPath: LinkedHashSet<String>? = null,
      ignoredAttrs: Set<String> = emptySet(),
      hashInvocationContext: HashInvocationContext = HashInvocationContext(),
  ): TargetDigest =
      hasher.digest(
          rule,
          allRulesMap,
          ruleHashes,
          sourceDigests,
          seedHash,
          packageBzlSeeds,
          depPath,
          ignoredAttrs,
          emptySet(),
          hashInvocationContext)

  @Test
  fun circularDependencySelfLoopThrows() {
    val a = rule("//pkg:a")
    assertFailure { digest(rule = a, depPath = linkedSetOf("//pkg:a")) }
        .isInstanceOf(RuleHasher.CircularDependencyException::class)
        .transform { it.message!! }
        .contains("//pkg:a -> //pkg:a")
  }

  @Test
  fun circularDependencyMultiHopThrows() {
    val a = rule("//pkg:a", ruleInputs = listOf("//pkg:b"))
    val b = rule("//pkg:b", ruleInputs = listOf("//pkg:a"))
    val rules = mapOf(a.name to a, b.name to b)

    assertFailure { digest(rule = a, allRulesMap = rules) }
        .isInstanceOf(RuleHasher.CircularDependencyException::class)
        .transform { it.message!! }
        .contains("//pkg:a -> //pkg:b -> //pkg:a")
  }

  @Test
  fun memoizationReturnsCachedDigest() {
    val leaf = rule("//pkg:leaf")
    val ruleHashes = ConcurrentHashMap<String, TargetDigest>()
    val first = digest(rule = leaf, ruleHashes = ruleHashes)
    assertThat(first.overallDigest.toHexString())
        .isEqualTo(ruleHashes[leaf.name]!!.overallDigest.toHexString())

    val cached =
        TargetDigest("cached-overall".toByteArray(), "cached-direct".toByteArray(), emptyList())
    ruleHashes[leaf.name] = cached

    val second = digest(rule = leaf, ruleHashes = ruleHashes)
    assertThat(second.overallDigest.toHexString()).isEqualTo(cached.overallDigest.toHexString())
    assertThat(second.directDigest.toHexString()).isEqualTo(cached.directDigest.toHexString())
    assertThat(second.overallDigest.toHexString()).isNotEqualTo(first.overallDigest.toHexString())
  }

  @Test
  fun emptyInstantiationStackFallsBackToPackageBzlSeeds() {
    val leaf = rule("//pkg:leaf")
    val withSeed =
        digest(rule = leaf, packageBzlSeeds = mapOf("//pkg" to "package-seed-v1".toByteArray()))
    val withOtherSeed =
        digest(rule = leaf, packageBzlSeeds = mapOf("//pkg" to "package-seed-v2".toByteArray()))
    val withoutSeed = digest(rule = leaf, packageBzlSeeds = emptyMap())

    assertThat(withSeed.overallDigest.toHexString())
        .isNotEqualTo(withOtherSeed.overallDigest.toHexString())
    assertThat(withSeed.overallDigest.toHexString())
        .isNotEqualTo(withoutSeed.overallDigest.toHexString())
  }

  @Test
  fun nonEmptyInstantiationStackUsesRuleBzlSeedAndFiltersExternalPaths() {
    // Main-repo .bzl/.scl frames contribute; external/@/../ frames are ignored.
    val withMainRepo =
        rule(
            "//pkg:lib",
            instantiationStack =
                listOf(
                    "tools/macro.bzl:1:1: my_macro",
                    "defs/helper.scl:2:3: helper",
                    "external/foo.bzl:1:1: ext",
                    "@repo//bar.bzl:1:1: remote",
                    "../outside.bzl:1:1: parent",
                    "BUILD:1:1: <toplevel>",
                ))
    val externalOnly =
        rule(
            "//pkg:lib",
            instantiationStack =
                listOf(
                    "external/foo.bzl:1:1: ext",
                    "@repo//bar.bzl:1:1: remote",
                    "../outside.bzl:1:1: parent",
                ))

    fake.softDigestValue = "bzl-v1".toByteArray()
    val mainV1 = digest(rule = withMainRepo)
    fake.softDigestValue = "bzl-v2".toByteArray()
    val mainV2 = digest(rule = withMainRepo)
    fake.softDigestValue = "bzl-v1".toByteArray()
    val externalV1 = digest(rule = externalOnly)
    fake.softDigestValue = "bzl-v2".toByteArray()
    val externalV2 = digest(rule = externalOnly)

    // Soft-digest of main-repo .bzl/.scl files is mixed in.
    assertThat(mainV1.overallDigest.toHexString()).isNotEqualTo(mainV2.overallDigest.toHexString())
    // Filtered-out frames produce a stable empty seed independent of softDigest.
    assertThat(externalV1.overallDigest.toHexString())
        .isEqualTo(externalV2.overallDigest.toHexString())
    // Non-null rule seed means packageBzlSeeds are NOT used as a fallback.
    val withPackageSeed =
        digest(
            rule = externalOnly,
            packageBzlSeeds = mapOf("//pkg" to "should-not-apply".toByteArray()))
    assertThat(withPackageSeed.overallDigest.toHexString())
        .isEqualTo(externalV1.overallDigest.toHexString())
  }

  @Test
  fun sourceDigestsUsedWhenInputIsNotARule() {
    val consumer = rule("//pkg:consumer", ruleInputs = listOf("//pkg:src.txt"))
    val sources = ConcurrentHashMap<String, ByteArray>()
    sources["//pkg:src.txt"] = "src-digest-v1".toByteArray()

    val v1 = digest(rule = consumer, sourceDigests = sources)
    sources["//pkg:src.txt"] = "src-digest-v2".toByteArray()
    val v2 = digest(rule = consumer, sourceDigests = sources)

    assertThat(v1.overallDigest.toHexString()).isNotEqualTo(v2.overallDigest.toHexString())
  }

  @Test
  fun recursiveRuleDependencyChangesTransitiveDigest() {
    val depV1 = rule("//pkg:dep")
    val depV2 =
        BazelRule(
            Build.Rule.newBuilder()
                .setName("//pkg:dep")
                .setRuleClass("sh_test")
                .addAttribute(
                    Attribute.newBuilder()
                        .setName("cmd")
                        .setType(Attribute.Discriminator.STRING)
                        .setStringValue("changed")
                        .build())
                .build())
    val consumer = rule("//pkg:consumer", ruleInputs = listOf("//pkg:dep"))

    val hashV1 =
        digest(rule = consumer, allRulesMap = mapOf(consumer.name to consumer, depV1.name to depV1))
    val hashV2 =
        digest(rule = consumer, allRulesMap = mapOf(consumer.name to consumer, depV2.name to depV2))

    assertThat(hashV1.overallDigest.toHexString()).isNotEqualTo(hashV2.overallDigest.toHexString())
    // Direct digest excludes transitive dep content.
    assertThat(hashV1.directDigest.toHexString()).isEqualTo(hashV2.directDigest.toHexString())
  }

  @Test
  fun heuristicSoftDigestLogsInfoAndCachesSourceDigest() {
    val logger = declareMock<Logger>()
    val consumer = rule("//pkg:consumer", ruleInputs = listOf("//pkg:unknown.src"))
    val sourceDigests = ConcurrentHashMap<String, ByteArray>()
    fake.softDigestValue = "heuristic-bytes".toByteArray()

    val result = digest(rule = consumer, sourceDigests = sourceDigests)

    assertThat(result).isNotNull()
    assertThat(sourceDigests["//pkg:unknown.src"]!!.toHexString())
        .isEqualTo("heuristic-bytes".toByteArray().toHexString())
    assertThat(fake.softDigestCalls.get()).isEqualTo(1)

    val infoCaptor = argumentCaptor<() -> String>()
    verify(logger, atLeastOnce()).i(infoCaptor.capture())
    assertThat(infoCaptor.allValues.any { it().contains("//pkg:unknown.src") }).isEqualTo(true)
  }

  @Test
  fun nullSoftDigestLogsWarning() {
    val logger = declareMock<Logger>()
    val consumer = rule("//pkg:consumer", ruleInputs = listOf("//pkg:missing.src"))
    fake.softDigestValue = null

    digest(rule = consumer)

    val warnCaptor = argumentCaptor<() -> String>()
    verify(logger, atLeastOnce()).w(warnCaptor.capture())
    assertThat(warnCaptor.allValues.any { it().contains("//pkg:missing.src") }).isEqualTo(true)
  }

  @Test
  fun packageGroupVisibilityEdgesAreFollowed() {
    val group = packageGroup("//pkg:consumers", packages = listOf("//allowed"))
    val gated = rule("//pkg:gated", visibility = listOf("//pkg:consumers", "//visibility:public"))
    val rules = mapOf(gated.name to gated, group.name to group)

    val hashWithGroup = digest(rule = gated, allRulesMap = rules)
    val hashWithoutGroup = digest(rule = gated, allRulesMap = mapOf(gated.name to gated))
    assertThat(hashWithGroup.overallDigest.toHexString())
        .isNotEqualTo(hashWithoutGroup.overallDigest.toHexString())

    // Changing the package_group attribute changes the gated rule's transitive digest.
    val groupChanged = packageGroup("//pkg:consumers", packages = listOf("//other"))
    val hashChanged =
        digest(
            rule = gated,
            allRulesMap = mapOf(gated.name to gated, groupChanged.name to groupChanged))
    assertThat(hashChanged.overallDigest.toHexString())
        .isNotEqualTo(hashWithGroup.overallDigest.toHexString())
  }

  @Test
  fun packageGroupVisibilitySkippedWhenVisibilityIgnored() {
    val group = packageGroup("//pkg:consumers")
    val gated = rule("//pkg:gated", visibility = listOf("//pkg:consumers"))
    val rules = mapOf(gated.name to gated, group.name to group)

    val followed = digest(rule = gated, allRulesMap = rules, ignoredAttrs = emptySet())
    val ignored = digest(rule = gated, allRulesMap = rules, ignoredAttrs = setOf("visibility"))
    // Ignoring visibility both drops the attribute from rule.digest and skips the edge walk.
    assertThat(followed.overallDigest.toHexString())
        .isNotEqualTo(ignored.overallDigest.toHexString())

    val ignoredWithoutGroup =
        digest(
            rule = gated,
            allRulesMap = mapOf(gated.name to gated),
            ignoredAttrs = setOf("visibility"))
    assertThat(ignored.overallDigest.toHexString())
        .isEqualTo(ignoredWithoutGroup.overallDigest.toHexString())
  }

  @Test
  fun packageGroupVisibilitySkipsMissingAndNonPackageGroupLabels() {
    val notAGroup = rule("//pkg:not_a_group")
    val gated =
        rule(
            "//pkg:gated",
            visibility = listOf("//pkg:missing_group", "//pkg:not_a_group", "//pkg:gated"))
    // Self label filtered by name==rule.name; missing continues; non-package_group continues.
    val result =
        digest(rule = gated, allRulesMap = mapOf(gated.name to gated, notAGroup.name to notAGroup))
    assertThat(result.deps).isEqualTo(emptyList())
  }

  @Test
  fun useCqueryMixesConfiguredRuleInputEncoding() {
    val dep = rule("//pkg:dep")
    val consumerA = rule("//pkg:consumer", configuredRuleInputs = listOf("//pkg:dep" to "cfg-A"))
    val consumerB = rule("//pkg:consumer", configuredRuleInputs = listOf("//pkg:dep" to "cfg-B"))
    val hasher = hasher(useCquery = true)

    val hashA =
        digest(
            hasher = hasher,
            rule = consumerA,
            allRulesMap = mapOf(consumerA.name to consumerA, dep.name to dep))
    val hashB =
        digest(
            hasher = hasher,
            rule = consumerB,
            allRulesMap = mapOf(consumerB.name to consumerB, dep.name to dep))

    assertThat(hashA.overallDigest.toHexString()).isNotEqualTo(hashB.overallDigest.toHexString())
    // Bare label is tracked in deps (checksum stripped).
    assertThat(hashA.deps).isEqualTo(listOf("//pkg:dep"))
  }

  @Test
  fun trackDepLabelsPopulatesDeps() {
    val dep = rule("//pkg:dep")
    val consumer = rule("//pkg:consumer", ruleInputs = listOf("//pkg:dep"))
    val rules = mapOf(consumer.name to consumer, dep.name to dep)

    val tracked =
        digest(hasher = hasher(trackDepLabels = true), rule = consumer, allRulesMap = rules)
    val untracked =
        digest(hasher = hasher(trackDepLabels = false), rule = consumer, allRulesMap = rules)

    assertThat(tracked.deps).isEqualTo(listOf("//pkg:dep"))
    assertThat(untracked.deps).isNull()
  }

  @Test
  fun selfReferenceInRuleInputsSkippedSafely() {
    val self = rule("//pkg:self", ruleInputs = listOf("//pkg:self"))
    fake.softDigestValue = null

    // Does not recurse into itself (would cycle); falls through to heuristic and completes.
    val result = digest(rule = self, allRulesMap = mapOf(self.name to self))
    assertThat(result.overallDigest).isNotNull()
    assertThat(result.deps).isEqualTo(emptyList())
  }

  @Test
  fun alwaysAffectedSeedIsMixedIntoDirectDigest() {
    val tagged = rule("//pkg:lint", tags = listOf("external"))
    val a =
        digest(
            hasher =
                hasher(
                    alwaysAffectedTags = setOf("external"),
                    alwaysAffectedSeed = "seed-A".toByteArray()),
            rule = tagged)
    val b =
        digest(
            hasher =
                hasher(
                    alwaysAffectedTags = setOf("external"),
                    alwaysAffectedSeed = "seed-B".toByteArray()),
            rule = tagged)

    assertThat(a.overallDigest.toHexString()).isNotEqualTo(b.overallDigest.toHexString())
    assertThat(a.directDigest.toHexString()).isNotEqualTo(b.directDigest.toHexString())
  }
}
