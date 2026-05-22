package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Attribute
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Rule
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

  // Fix for https://github.com/Tinder/bazel-diff/issues/359
  //
  // Under --useCquery, BazelRule.ruleInputList() now folds each ConfiguredRuleInput's
  // configurationChecksum into the rule-input string via CONFIGURED_RULE_INPUT_SEPARATOR (`|`).
  // Two configured graphs that share dep labels but differ in per-edge configuration -- a
  // `cfg = "exec"` transition flip, a --platforms swap, a --config=... change, or a `--define`
  // toggling a select() inside a dep -- produce distinct ruleInputList() output, so RuleHasher's
  // transitive walk mixes those bytes into the digest and the parent target's hash changes.
  // RuleHasher decodes back to the bare label when looking the input up in `allRulesMap` /
  // `sourceDigests` and when tracking deps, so the user-visible deps output stays clean.
  @Test
  fun testCqueryRuleInputListDistinguishesConfigurationChecksum_issue359() {
    val ruleA = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-A")
    val ruleB = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-B")

    val inputsA =
        BazelRule(ruleA).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())
    val inputsB =
        BazelRule(ruleB).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputsA).isNotEqualTo(inputsB)
  }

  // Pins the on-the-wire encoding so the format is observable and future changes are intentional.
  // The `|` separator is invalid in a Bazel label, so the encoded string cannot collide with any
  // real label and can be safely round-tripped by RuleHasher's `decodeConfiguredRuleInputLabel`.
  @Test
  fun testCqueryRuleInputListEncodesLabelAndChecksum_issue359() {
    val rule = configuredGenrule(depLabel = "//:dep", configurationChecksum = "cfg-checksum-A")

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs).isEqualTo(listOf("//:dep|cfg-checksum-A"))
  }

  // The empty-checksum fallback (a known Bazel cquery quirk) must NOT append a dangling separator
  // -- we drop back to a bare label so RuleHasher's lookup keeps working unchanged.
  @Test
  fun testCqueryRuleInputListEmptyChecksumFallsBackToBareLabel_issue359() {
    val rule = configuredGenrule(depLabel = "//:dep", configurationChecksum = "")

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs).isEqualTo(listOf("//:dep"))
  }

  // Without --useCquery, configured_rule_input is irrelevant: the legacy ruleInputList field is
  // the source of truth, so the encoding must not leak into the non-cquery path.
  @Test
  fun testNonCqueryRuleInputListIgnoresConfiguredRuleInput_issue359() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("genrule")
            .setName("//:gen")
            .addRuleInput("//:legacy_dep")
            .addConfiguredRuleInput(
                Build.ConfiguredRuleInput.newBuilder()
                    .setLabel("//:dep")
                    .setConfigurationChecksum("cfg-checksum-A")
                    .build())
            .build()

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = false, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs).isEqualTo(listOf("//:legacy_dep"))
  }

  // Pins the round-trip behaviour `RuleHasher` relies on: the full encoded string lives in the
  // hash, and the bare label is what gets looked up in `allRulesMap` / `sourceDigests` and
  // tracked in `deps`. If the round-trip ever drifts the user-facing JSON would start emitting
  // dep labels with `|<checksum>` suffixes.
  @Test
  fun testDecodeConfiguredRuleInputLabelRoundTrip_issue359() {
    val withChecksum =
        encodeConfiguredRuleInput(
            Build.ConfiguredRuleInput.newBuilder()
                .setLabel("//:dep")
                .setConfigurationChecksum("cfg-A")
                .build())
    assertThat(decodeConfiguredRuleInputLabel(withChecksum)).isEqualTo("//:dep")

    val withoutChecksum =
        encodeConfiguredRuleInput(Build.ConfiguredRuleInput.newBuilder().setLabel("//:dep").build())
    assertThat(decodeConfiguredRuleInputLabel(withoutChecksum)).isEqualTo("//:dep")

    // Non-encoded labels (the non-cquery path or //external:* synthetic inputs) must round-trip
    // unchanged so the decode is safe to call unconditionally in RuleHasher.
    assertThat(decodeConfiguredRuleInputLabel("//:bare")).isEqualTo("//:bare")
    assertThat(decodeConfiguredRuleInputLabel("//external:foo")).isEqualTo("//external:foo")
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
