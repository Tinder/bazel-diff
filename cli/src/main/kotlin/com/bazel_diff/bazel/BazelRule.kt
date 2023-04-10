package com.bazel_diff.bazel

import com.bazel_diff.hash.safePutBytes
import com.bazel_diff.hash.sha256
import com.google.devtools.build.lib.query2.proto.proto2api.Build

// Ignore generator_location when computing a target's hash since it is likely to change and does not
// affect a target's generated actions. Internally, Bazel also does this when computing a target's hash:
// https://github.com/bazelbuild/bazel/blob/6971b016f1e258e3bb567a0f9fe7a88ad565d8f2/src/main/java/com/google/devtools/build/lib/query2/query/output/SyntheticAttributeHashCalculator.java#L78-L81
private val IGNORED_ATTRS = arrayOf("generator_location")

class BazelRule(private val rule: Build.Rule) {
    val digest: ByteArray by lazy {
        sha256 {
            safePutBytes(rule.ruleClassBytes.toByteArray())
            safePutBytes(rule.nameBytes.toByteArray())
            safePutBytes(rule.skylarkEnvironmentHashCodeBytes.toByteArray())
            for (attribute in rule.attributeList) {
                if (!IGNORED_ATTRS.contains(attribute.name))
                    safePutBytes(attribute.toByteArray())
            }
        }
    }

    fun ruleInputList(fineGrainedHashExternalRepos: Set<String>): List<String> {
        return rule.ruleInputList.map { ruleInput: String -> transformRuleInput(fineGrainedHashExternalRepos, ruleInput) }
    }

    val name: String = rule.name

    private fun transformRuleInput(fineGrainedHashExternalRepos: Set<String>, ruleInput: String): String {
        if (ruleInput.startsWith("@") && fineGrainedHashExternalRepos.none { ruleInput.startsWith("@$it") }) {
            val splitRule = ruleInput.split("//".toRegex()).dropLastWhile { it.isEmpty() }.toTypedArray()
            if (splitRule.size == 2) {
                var externalRule = splitRule[0]
                externalRule = externalRule.replaceFirst("@".toRegex(), "")
                return String.format("//external:%s", externalRule)
            }
        }
        return ruleInput
    }
}
