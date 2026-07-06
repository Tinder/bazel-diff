package com.bazel_diff.hash

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.testModule
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Attribute
import java.util.concurrent.ConcurrentHashMap
import org.junit.Rule
import org.junit.Test
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule

/**
 * Tests for `--alwaysAffectedTags` (issue [#401](https://github.com/Tinder/bazel-diff/issues/401)).
 *
 * A target carrying one of the configured tags may read undeclared workspace state at execution
 * time, so its declared inputs cannot make its hash change when that state does. [RuleHasher] mixes
 * a per-invocation sentinel into such a target's digest so two `generate-hashes` runs never agree
 * and a base/head diff always marks it impacted -- while every other target's hash stays
 * byte-for-byte stable.
 */
class RuleHasherAlwaysAffectedTagsTest : KoinTest {
  @get:Rule val koinTestRule = KoinTestRule.create { modules(testModule()) }

  private fun ruleWith(name: String, vararg tags: String): BazelRule {
    val builder = Build.Rule.newBuilder().setName(name).setRuleClass("sh_test")
    if (tags.isNotEmpty()) {
      val attr = Attribute.newBuilder().setName("tags").setType(Attribute.Discriminator.STRING_LIST)
      tags.forEach { attr.addStringListValue(it) }
      builder.addAttribute(attr.build())
    }
    return BazelRule(builder.build())
  }

  /** Hashes [rule] with a fresh, unmemoized [RuleHasher] for the given flag configuration. */
  private fun digest(
      rule: BazelRule,
      alwaysAffectedTags: Set<String>,
      seed: ByteArray
  ): TargetDigest {
    val hasher =
        RuleHasher(
            useCquery = false,
            trackDepLabels = true,
            fineGrainedHashExternalRepos = emptySet(),
            alwaysAffectedTags = alwaysAffectedTags,
            alwaysAffectedSeed = seed)
    return hasher.digest(
        rule,
        mapOf(rule.name to rule),
        ConcurrentHashMap(),
        ConcurrentHashMap(),
        ByteArray(0),
        emptyMap(),
        null,
        emptySet(),
        emptySet())
  }

  @Test
  fun taggedTargetHashChangesAcrossInvocations() {
    val rule = ruleWith("//pkg:lint", "external")
    val a = digest(rule, setOf("external"), "seed-A".toByteArray()).overallDigest.toHexString()
    val b = digest(rule, setOf("external"), "seed-B".toByteArray()).overallDigest.toHexString()
    // Two runs mint different sentinels, so the tagged target never hashes the same twice.
    assertThat(a).isNotEqualTo(b)
  }

  @Test
  fun taggedTargetDirectHashChangesAcrossInvocations() {
    // The sentinel is mixed into the DIRECT digest so distance metrics classify the target as
    // DIRECT-impacted rather than requiring an impacted dependency to explain it.
    val rule = ruleWith("//pkg:lint", "external")
    val a = digest(rule, setOf("external"), "seed-A".toByteArray()).directDigest.toHexString()
    val b = digest(rule, setOf("external"), "seed-B".toByteArray()).directDigest.toHexString()
    assertThat(a).isNotEqualTo(b)
  }

  @Test
  fun untaggedTargetHashIsStableAcrossInvocations() {
    val rule = ruleWith("//pkg:lib") // no tags
    val a = digest(rule, setOf("external"), "seed-A".toByteArray()).overallDigest.toHexString()
    val b = digest(rule, setOf("external"), "seed-B".toByteArray()).overallDigest.toHexString()
    assertThat(a).isEqualTo(b)
  }

  @Test
  fun targetWithAnUnlistedTagIsStableAcrossInvocations() {
    // A target tagged `no-cache` but not `external` is not opted in, so it stays stable.
    val rule = ruleWith("//pkg:lint", "no-cache")
    val a = digest(rule, setOf("external"), "seed-A".toByteArray()).overallDigest.toHexString()
    val b = digest(rule, setOf("external"), "seed-B".toByteArray()).overallDigest.toHexString()
    assertThat(a).isEqualTo(b)
  }

  @Test
  fun enablingFlagDoesNotPerturbUntaggedTargets() {
    // Turning the feature on must leave a target that lacks the tag byte-for-byte identical to the
    // feature-off baseline -- otherwise the whole graph would over-invalidate.
    val rule = ruleWith("//pkg:lib")
    val off = digest(rule, emptySet(), ByteArray(0)).overallDigest.toHexString()
    val on = digest(rule, setOf("external"), "seed-A".toByteArray()).overallDigest.toHexString()
    assertThat(on).isEqualTo(off)
  }

  @Test
  fun taggedTargetMatchesBaselineWhenFeatureDisabled() {
    // With no configured tags the sentinel is never mixed, even for a would-be-tagged target.
    val rule = ruleWith("//pkg:lint", "external")
    val a = digest(rule, emptySet(), "seed-A".toByteArray()).overallDigest.toHexString()
    val b = digest(rule, emptySet(), "seed-B".toByteArray()).overallDigest.toHexString()
    assertThat(a).isEqualTo(b)
  }

  @Test
  fun anyMatchingTagOptsTheTargetIn() {
    // The target carries several tags; matching any one of the configured set is enough.
    val rule = ruleWith("//pkg:lint", "no-cache", "external", "requires-network")
    val a = digest(rule, setOf("external"), "seed-A".toByteArray()).overallDigest.toHexString()
    val b = digest(rule, setOf("external"), "seed-B".toByteArray()).overallDigest.toHexString()
    assertThat(a).isNotEqualTo(b)
  }
}
