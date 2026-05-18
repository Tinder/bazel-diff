package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.isEqualTo
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import org.junit.Test

class BazelTargetTest {
  // Helper that builds a Rule-shaped target with a chosen discriminator so we can
  // exercise the `type` getter's branches without needing six separate hand-built
  // protos. `BazelTarget.Rule` only asserts `target.hasRule()` in its init, which
  // is satisfied by setting any Rule on the builder, so we can drive any
  // discriminator through it.
  private fun ruleTargetWith(d: Build.Target.Discriminator): Build.Target =
      Build.Target.newBuilder()
          .setType(d)
          .setRule(Build.Rule.newBuilder().setRuleClass("dummy").setName("//:dummy"))
          .build()

  @Test
  fun sourceFileTarget_exposesSourceFileName_andSourceFileType() {
    val target = Build.Target.newBuilder()
        .setType(Build.Target.Discriminator.SOURCE_FILE)
        .setSourceFile(
            Build.SourceFile.newBuilder()
                .setName("//pkg:foo.txt")
                .addSubinclude("//other:bzl.bzl"))
        .build()
    val bt = BazelTarget.SourceFile(target)
    assertThat(bt.name).isEqualTo("//pkg:foo.txt")
    assertThat(bt.sourceFileName).isEqualTo("//pkg:foo.txt")
    assertThat(bt.subincludeList).isEqualTo(listOf("//other:bzl.bzl"))
    assertThat(bt.type).isEqualTo(BazelTargetType.SOURCE_FILE)
  }

  @Test
  fun ruleTarget_exposesRuleName_andRuleType() {
    val target = Build.Target.newBuilder()
        .setType(Build.Target.Discriminator.RULE)
        .setRule(Build.Rule.newBuilder().setRuleClass("java_library").setName("//:foo"))
        .build()
    val bt = BazelTarget.Rule(target)
    assertThat(bt.name).isEqualTo("//:foo")
    assertThat(bt.rule.name).isEqualTo("//:foo")
    assertThat(bt.type).isEqualTo(BazelTargetType.RULE)
  }

  @Test
  fun generatedFileTarget_exposesNamesAndGeneratedFileType() {
    val target = Build.Target.newBuilder()
        .setType(Build.Target.Discriminator.GENERATED_FILE)
        .setGeneratedFile(
            Build.GeneratedFile.newBuilder()
                .setName("//:gen.out")
                .setGeneratingRule("//:gen"))
        .build()
    val bt = BazelTarget.GeneratedFile(target)
    assertThat(bt.name).isEqualTo("//:gen.out")
    assertThat(bt.generatedFileName).isEqualTo("//:gen.out")
    assertThat(bt.generatingRuleName).isEqualTo("//:gen")
    assertThat(bt.type).isEqualTo(BazelTargetType.GENERATED_FILE)
  }

  @Test
  fun packageGroupDiscriminator_mapsToPackageGroupType() {
    val bt = BazelTarget.Rule(ruleTargetWith(Build.Target.Discriminator.PACKAGE_GROUP))
    assertThat(bt.type).isEqualTo(BazelTargetType.PACKAGE_GROUP)
  }

  @Test
  fun environmentGroupDiscriminator_mapsToEnvironmentGroupType() {
    val bt = BazelTarget.Rule(ruleTargetWith(Build.Target.Discriminator.ENVIRONMENT_GROUP))
    assertThat(bt.type).isEqualTo(BazelTargetType.ENVIRONMENT_GROUP)
  }
}
