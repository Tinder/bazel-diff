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

  // Backs --alwaysAffectedTags (issue #401): the `tags` attribute is emitted as a STRING_LIST.
  @Test
  fun testTagsExtractedFromAttribute() {
    val rulePb =
        Rule.newBuilder()
            .setRuleClass("sh_test")
            .setName("//pkg:lint")
            .addAttribute(
                Attribute.newBuilder()
                    .setType(Attribute.Discriminator.STRING_LIST)
                    .setName("tags")
                    .addStringListValue("external")
                    .addStringListValue("no-cache")
                    .build())
            .build()

    assertThat(BazelRule(rulePb).tags()).isEqualTo(setOf("external", "no-cache"))
  }

  @Test
  fun testTagsEmptyWhenAttributeAbsent() {
    val rulePb = Rule.newBuilder().setRuleClass("java_library").setName("//pkg:lib").build()

    assertThat(BazelRule(rulePb).tags()).isEqualTo(emptySet<String>())
  }

  // Backs package_group support (issue #441): only real package_group labels survive the
  // filter -- the special `//visibility:*` forms and `__pkg__`/`__subpackages__` package specs
  // name no target of their own.
  @Test
  fun testVisibilityPackageGroupLabelsFiltersSpecialForms() {
    val rulePb =
        Rule.newBuilder()
            .setRuleClass("genrule")
            .setName("//lib:thing")
            .addAttribute(
                Attribute.newBuilder()
                    .setType(Attribute.Discriminator.STRING_LIST)
                    .setName("visibility")
                    .addStringListValue("//visibility:public")
                    .addStringListValue("//other:__pkg__")
                    .addStringListValue("//other:__subpackages__")
                    .addStringListValue("//lib:zgroup")
                    .addStringListValue("//lib:consumers")
                    .addStringListValue("//lib:consumers")
                    .build())
            .build()

    // Deduped and sorted for a deterministic digest contribution.
    assertThat(BazelRule(rulePb).visibilityPackageGroupLabels())
        .isEqualTo(listOf("//lib:consumers", "//lib:zgroup"))
  }

  @Test
  fun testVisibilityPackageGroupLabelsEmptyWhenAttributeAbsent() {
    val rulePb = Rule.newBuilder().setRuleClass("java_library").setName("//pkg:lib").build()

    assertThat(BazelRule(rulePb).visibilityPackageGroupLabels()).isEqualTo(emptyList<String>())
  }

  @Test
  fun testIsPackageGroup() {
    val packageGroupPb = Rule.newBuilder().setRuleClass("package_group").setName("//lib:pg").build()
    val rulePb = Rule.newBuilder().setRuleClass("java_library").setName("//pkg:lib").build()

    assertThat(BazelRule(packageGroupPb).isPackageGroup).isEqualTo(true)
    assertThat(BazelRule(rulePb).isPackageGroup).isEqualTo(false)
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

  // Hub-and-spoke external repos (e.g. rules_python's `@pip` hub with one `@pip_<pkg>` spoke
  // repo per package): a spoke label shares the hub name as a string prefix but is a different
  // repo, so marking the hub fine-grained must not stop spoke inputs from being rewritten to
  // their `//external:<spoke>` synthetic target -- that target is the only node whose hash
  // flips when the spoke's pinned version changes.
  @Test
  fun testFineGrainedRepoNameDoesNotPrefixMatchSpokeRepos() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("alias")
            .setName("@pip//numpy:pkg")
            .addRuleInput("@pip_numpy//:pkg")
            .addRuleInput("@pip//numpy:other")
            .build()

    val inputs =
        BazelRule(rule)
            .ruleInputList(useCquery = false, fineGrainedHashExternalRepos = setOf("@pip"))

    // The spoke input is rewritten to its seed; the hub-internal input stays raw because the
    // hub itself is fine-grained.
    assertThat(inputs.contains("//external:pip_numpy")).isEqualTo(true)
    assertThat(inputs.contains("//external:pip")).isEqualTo(false)
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

  // Canonicalization (mirrors target-determinator commit d4b6125): a rule's digest must be
  // invariant to the order Bazel happens to emit attributes in. Attribute names are unique within
  // a rule, so the same set of attributes in a different order is the same rule and must hash the
  // same -- otherwise a Bazel upgrade (or any reordering) would spuriously invalidate every target.
  @Test
  fun testDigestInvariantToAttributeOrder() {
    val attrA =
        Attribute.newBuilder()
            .setType(Attribute.Discriminator.STRING)
            .setName("a_attr")
            .setStringValue("1")
            .build()
    val attrZ =
        Attribute.newBuilder()
            .setType(Attribute.Discriminator.STRING)
            .setName("z_attr")
            .setStringValue("2")
            .build()

    val forward =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("lib")
            .addAttribute(attrA)
            .addAttribute(attrZ)
            .build()
    val reversed =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("lib")
            .addAttribute(attrZ)
            .addAttribute(attrA)
            .build()

    assertThat(BazelRule(forward).digest(emptySet()))
        .isEqualTo(BazelRule(reversed).digest(emptySet()))
  }

  // Sorting attributes must not erase sensitivity to an actual attribute-value change.
  @Test
  fun testDigestStillDetectsAttributeValueChange() {
    fun ruleWith(value: String) =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("lib")
            .addAttribute(
                Attribute.newBuilder()
                    .setType(Attribute.Discriminator.STRING)
                    .setName("a_attr")
                    .setStringValue(value)
                    .build())
            .build()

    assertThat(BazelRule(ruleWith("before")).digest(emptySet()))
        .isNotEqualTo(BazelRule(ruleWith("after")).digest(emptySet()))
  }

  // Rule inputs are a set, not a sequence: the non-cquery input list must come back deduped and in
  // a deterministic (sorted) order regardless of Bazel's emission order.
  @Test
  fun testNonCqueryRuleInputListDedupesAndSorts() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("genrule")
            .setName("//:gen")
            .addRuleInput("//:b")
            .addRuleInput("//:a")
            .addRuleInput("//:b")
            .build()

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = false, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs).isEqualTo(listOf("//:a", "//:b"))
  }

  // The configuration-aware (#359) path is the one most exposed to non-deterministic emission
  // order: cquery can surface the same dep label across multiple configurations in any order. Two
  // graphs that differ only by that order describe the same target and must produce an identical
  // ruleInputList(), or RuleHasher would report a spurious change.
  @Test
  fun testCqueryRuleInputListInvariantToConfiguredInputOrder() {
    fun genruleWith(inputs: List<Build.ConfiguredRuleInput>): Rule {
      val builder = Rule.newBuilder().setRuleClass("genrule").setName("//:gen")
      inputs.forEach { builder.addConfiguredRuleInput(it) }
      return builder.build()
    }

    val inputA =
        Build.ConfiguredRuleInput.newBuilder()
            .setLabel("//:a")
            .setConfigurationChecksum("cfg-A")
            .build()
    val inputB =
        Build.ConfiguredRuleInput.newBuilder()
            .setLabel("//:b")
            .setConfigurationChecksum("cfg-B")
            .build()

    val forward =
        BazelRule(genruleWith(listOf(inputA, inputB)))
            .ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())
    // Reversed order, plus a duplicate of inputA to exercise dedupe.
    val reversed =
        BazelRule(genruleWith(listOf(inputB, inputA, inputA)))
            .ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())

    assertThat(forward).isEqualTo(listOf("//:a|cfg-A", "//:b|cfg-B"))
    assertThat(forward).isEqualTo(reversed)
  }

  // External @repo//... inputs that are not in fineGrainedHashExternalRepos collapse to a
  // synthetic //external:<repo> label so dependents pick up WORKSPACE/bzlmod repo hash changes.
  @Test
  fun testExternalRepoInputCollapsesToSyntheticExternalTarget() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("//pkg:lib")
            .addRuleInput("@com_google_guava//guava:guava")
            .addRuleInput("//pkg:local")
            .build()

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = false, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs)
        .isEqualTo(
            listOf("//external:com_google_guava", "//pkg:local", "@com_google_guava//guava:guava"))
  }

  // Canonical @@repo labels strip all leading @ chars when collapsing.
  @Test
  fun testCanonicalExternalRepoInputCollapsesWithoutAtSigns() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("//pkg:lib")
            .addRuleInput("@@rules_jvm_external//:defs")
            .build()

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = false, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs)
        .isEqualTo(listOf("//external:rules_jvm_external", "@@rules_jvm_external//:defs"))
  }

  // Repos listed in fineGrainedHashExternalRepos keep their real labels; no //external:* synthetic.
  @Test
  fun testFineGrainedHashExternalReposPreservesExternalLabel() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("//pkg:lib")
            .addRuleInput("@inner_repo//:lib")
            .addRuleInput("@other_repo//:lib")
            .build()

    val inputs =
        BazelRule(rule)
            .ruleInputList(useCquery = false, fineGrainedHashExternalRepos = setOf("@inner_repo"))

    assertThat(inputs)
        .isEqualTo(listOf("//external:other_repo", "@inner_repo//:lib", "@other_repo//:lib"))
  }

  // Main-repo spellings must never be collapsed to //external:*.
  @Test
  fun testMainRepoInputsAreNotCollapsed() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("//pkg:lib")
            .addRuleInput("//pkg:dep")
            .addRuleInput("@//pkg:at_main")
            .addRuleInput("@@//pkg:canonical_main")
            .build()

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = false, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs).isEqualTo(listOf("//pkg:dep", "@//pkg:at_main", "@@//pkg:canonical_main"))
  }

  // Under cquery, synthetic //external:* inputs still come from rule_input (not
  // configured_rule_input).
  @Test
  fun testCqueryRuleInputListIncludesExternalSyntheticFromRuleInputs() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("genrule")
            .setName("//:gen")
            .addRuleInput("@ext//:lib")
            .addConfiguredRuleInput(
                Build.ConfiguredRuleInput.newBuilder()
                    .setLabel("//:dep")
                    .setConfigurationChecksum("cfg-A")
                    .build())
            .build()

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = true, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs).isEqualTo(listOf("//:dep|cfg-A", "//external:ext"))
  }

  // Labels that start with @ but lack a // package separator are left unchanged.
  @Test
  fun testExternalInputWithoutPackageSeparatorIsUnchanged() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("//pkg:lib")
            .addRuleInput("@nopackage")
            .build()

    val inputs =
        BazelRule(rule).ruleInputList(useCquery = false, fineGrainedHashExternalRepos = emptySet())

    assertThat(inputs).isEqualTo(listOf("@nopackage"))
  }

  @Test
  fun testInstantiationStackReturnsProtoFrames() {
    val rule =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("//pkg:lib")
            .addInstantiationStack("macros.bzl:10:2: my_macro")
            .addInstantiationStack("BUILD:5:1: <toplevel>")
            .build()

    assertThat(BazelRule(rule).instantiationStack)
        .isEqualTo(listOf("macros.bzl:10:2: my_macro", "BUILD:5:1: <toplevel>"))
  }

  @Test
  fun testInstantiationStackEmptyByDefault() {
    val rule = Rule.newBuilder().setRuleClass("java_library").setName("//pkg:lib").build()

    assertThat(BazelRule(rule).instantiationStack).isEqualTo(emptyList<String>())
  }

  // Custom ignoredAttrs are layered on top of DEFAULT_IGNORED_ATTRS (generator_location).
  @Test
  fun testDigestIgnoresCustomIgnoredAttrs() {
    fun ruleWith(tags: String) =
        Rule.newBuilder()
            .setRuleClass("java_library")
            .setName("lib")
            .addAttribute(
                Attribute.newBuilder()
                    .setType(Attribute.Discriminator.STRING)
                    .setName("tags")
                    .setStringValue(tags)
                    .build())
            .addAttribute(
                Attribute.newBuilder()
                    .setType(Attribute.Discriminator.STRING)
                    .setName("generator_location")
                    .setStringValue("BUILD:1:1")
                    .build())
            .build()

    val a = BazelRule(ruleWith("keep"))
    val b = BazelRule(ruleWith("drop"))

    assertThat(a.digest(setOf("tags"))).isEqualTo(b.digest(setOf("tags")))
    assertThat(a.digest(emptySet())).isNotEqualTo(b.digest(emptySet()))
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
