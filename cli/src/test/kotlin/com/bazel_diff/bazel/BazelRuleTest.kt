package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Attribute
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Rule
import org.junit.Ignore
import org.junit.Test

class BazelRuleTest {
  @Test
  fun testHashDiffers() {
    val rule1Pb = Rule.newBuilder().setRuleClass("java_library").setName("libfoo").build()

    val rule2Pb = Rule.newBuilder().setRuleClass("java_library").setName("libbar").build()
    assertThat(BazelRule(rule1Pb).digest(emptySet()))
        .isNotEqualTo(BazelRule(rule2Pb).digest(emptySet()))
  }

  @Test
  fun testIgnoreAttributes() {
    val rule1Pb =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("foo_library")
            .addAttribute(
                0,
                Attribute.newBuilder()
                    .setType(Attribute.Discriminator.STRING)
                    .setName("generator_location")
                    .setStringValue("path/to/BUILD:107:12")
                    .build())
            .build()

    val rule2Pb =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("foo_library")
            .addAttribute(
                0,
                Attribute.newBuilder()
                    .setType(Attribute.Discriminator.STRING)
                    .setName("generator_location")
                    .setStringValue("path/to/BUILD:111:1")
                    .build())
            .build()

    assertThat(BazelRule(rule1Pb).digest(emptySet()))
        .isEqualTo(BazelRule(rule2Pb).digest(emptySet()))
  }

  // Reproducer for https://github.com/Tinder/bazel-diff/issues/359
  //
  // Under --useCquery, BazelRule.ruleInputList() reads each entry from configured_rule_input but
  // only takes `.label`, discarding `.configurationChecksum`. Two configured graphs that share
  // dep labels but differ in per-edge configuration -- a `cfg = "exec"` transition flip, a
  // --platforms swap, a --config=... change, or a `--define` toggling a select() inside a dep --
  // produce identical ruleInputList() output, so RuleHasher's transitive walk visits the same
  // labels and yields identical digests. The user-visible symptom is that bazel-diff under
  // cquery returns an empty impacted-targets set despite a real change in the configured graph,
  // diverging from bazel-contrib/target-determinator which keys hashes by
  // (label, configurationChecksum) and mixes the dep-edge configuration into the rule digest.
  //
  // @Ignore on the assertion below pins the current (buggy) behaviour as a reproducer: the
  // companion `_currentBehaviourBuggy` test asserts the lists are equal, so the suite stays
  // green; the `@Ignore`d test documents the desired post-fix invariant and can be flipped on
  // (or merged with `_currentBehaviourBuggy`) when the fix lands.
  @Test
  @Ignore(
      "Reproducer for #359 -- expected to pass once cquery rule inputs are keyed by (label, configurationChecksum). Today the lists collapse and this assertion fails. The companion *_currentBehaviourBuggy_issue359 tests pin down the current behaviour so CI stays green; flipping this @Ignore off should be the second-to-last step of the fix PR.")
  fun testCqueryRuleInputListDistinguishesConfigurationChecksum_reproducerForIssue359() {
    val ruleA = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-A")
    val ruleB = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-B")

    val inputsA =
        BazelRule(ruleA).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())
    val inputsB =
        BazelRule(ruleB).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())

    // Today: both `[//:dep]` -- the test fails. After the fix it should pass.
    assertThat(inputsA).isNotEqualTo(inputsB)
  }

  // Companion test that locks in the *current* buggy behaviour so we have a regression signal:
  // if this test ever starts failing, it means the collapse has been (partially) fixed and the
  // `@Ignore`d assertion above should be revisited. This keeps the reproducer visible in source
  // without breaking CI.
  @Test
  fun testCqueryRuleInputListCollapsesConfigurationChecksum_currentBehaviourBuggy_issue359() {
    val ruleA = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-A")
    val ruleB = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-B")

    val inputsA =
        BazelRule(ruleA).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())
    val inputsB =
        BazelRule(ruleB).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())

    // Current (buggy) behaviour: both lists are exactly `[//:dep]`.
    assertThat(inputsA).isEqualTo(listOf("//:dep"))
    assertThat(inputsB).isEqualTo(listOf("//:dep"))
    assertThat(inputsA).isEqualTo(inputsB)
  }

  // Also pins the buggy behaviour at the digest level: two BazelRules that differ ONLY in the
  // dep-edge configuration checksum currently produce identical `digest(...)` output, because
  // configured_rule_input is not part of BazelRule.digest() at all (see BazelRule.digest()).
  @Test
  fun testBazelRuleDigestIgnoresConfigurationChecksum_currentBehaviourBuggy_issue359() {
    val ruleA = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-A")
    val ruleB = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-B")

    assertThat(BazelRule(ruleA).digest(emptySet())).isEqualTo(BazelRule(ruleB).digest(emptySet()))
  }

  private fun configuredGenrule(depLabel: String, configurationChecksum: String): Rule {
    return Rule.newBuilder()
        .setRuleClass("genrule")
        .setName("//:gen")
        .addConfiguredRuleInput(
            Build.ConfiguredRuleInput.newBuilder()
                .setLabel(depLabel)
                .setConfigurationChecksum(configurationChecksum)
                .build())
        .build()
  }
}
