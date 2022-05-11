package com.bazel_diff.bazel

import com.bazel_diff.hash.safePutBytes
import com.bazel_diff.hash.sha256
import com.google.devtools.build.lib.query2.proto.proto2api.Build

class BazelRule(private val rule: Build.Rule) {
    val digest: ByteArray by lazy {
        sha256 {
            safePutBytes(rule.ruleClassBytes.toByteArray())
            safePutBytes(rule.nameBytes.toByteArray())
            safePutBytes(rule.skylarkEnvironmentHashCodeBytes.toByteArray())
            for (attribute in rule.attributeList) {
                safePutBytes(attribute.toByteArray())
            }
        }
    }
    val ruleInputList: List<String>
        get() = rule.ruleInputList.map { ruleInput: String -> transformRuleInput(ruleInput) }

    val name: String = rule.name

    private fun transformRuleInput(ruleInput: String): String {
        if (ruleInput.startsWith("@")) {
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
