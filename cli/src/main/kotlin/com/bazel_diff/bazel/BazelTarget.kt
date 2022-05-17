package com.bazel_diff.bazel

import com.google.devtools.build.lib.query2.proto.proto2api.Build

sealed class BazelTarget(private val target: Build.Target) {
    class SourceFile(target: Build.Target) : BazelTarget(target) {
        init {
            assert(target.hasSourceFile())
        }

        val sourceFileName: String = target.sourceFile.name
        override val name: String
            get() = sourceFileName
    }

    class Rule(target: Build.Target) : BazelTarget(target) {
        init {
            assert(target.hasRule())
        }

        val rule: BazelRule = BazelRule(target.rule)
        override val name: String
            get() = rule.name
    }

    class GeneratedFile(target: Build.Target) : BazelTarget(target) {
        init {
            assert(target.hasGeneratedFile())
        }

        val generatedFileName: String = target.generatedFile.name
        val generatingRuleName: String = target.generatedFile.generatingRule
        override val name: String
            get() = generatedFileName
    }

    val type: BazelTargetType
        get() = when (target.type) {
            Build.Target.Discriminator.RULE -> BazelTargetType.RULE
            Build.Target.Discriminator.SOURCE_FILE -> BazelTargetType.SOURCE_FILE
            Build.Target.Discriminator.GENERATED_FILE -> BazelTargetType.GENERATED_FILE
            Build.Target.Discriminator.PACKAGE_GROUP -> BazelTargetType.PACKAGE_GROUP
            Build.Target.Discriminator.ENVIRONMENT_GROUP -> BazelTargetType.ENVIRONMENT_GROUP
            else -> BazelTargetType.UNKNOWN
        }

    abstract val name: String
}

