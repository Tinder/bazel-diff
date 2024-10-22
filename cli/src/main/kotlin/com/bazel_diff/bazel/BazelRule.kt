package com.bazel_diff.bazel

import com.bazel_diff.hash.safePutBytes
import com.bazel_diff.hash.sha256
import com.google.devtools.build.lib.query2.proto.proto2api.Build

// Ignore generator_location when computing a target's hash since it is likely to change and does
// not
// affect a target's generated actions. Internally, Bazel also does this when computing a target's
// hash:
// https://github.com/bazelbuild/bazel/blob/6971b016f1e258e3bb567a0f9fe7a88ad565d8f2/src/main/java/com/google/devtools/build/lib/query2/query/output/SyntheticAttributeHashCalculator.java#L78-L81
private val DEFAULT_IGNORED_ATTRS = arrayOf("generator_location")

class BazelRule(private val rule: Build.Rule) {
  fun digest(ignoredAttrs: Set<String>): ByteArray {
    return sha256 {
      safePutBytes(rule.ruleClassBytes.toByteArray())
      safePutBytes(rule.nameBytes.toByteArray())
      safePutBytes(rule.skylarkEnvironmentHashCodeBytes.toByteArray())
      for (attribute in rule.attributeList) {
        if (!DEFAULT_IGNORED_ATTRS.contains(attribute.name) &&
            !ignoredAttrs.contains(attribute.name))
            safePutBytes(attribute.toByteArray())
      }
    }
  }

  fun ruleInputList(useCquery: Boolean, fineGrainedHashExternalRepos: Set<String>): List<String> {
    return if (useCquery) {
      rule.configuredRuleInputList.map { it.label } +
          rule.ruleInputList
              .map { ruleInput: String ->
                transformRuleInput(fineGrainedHashExternalRepos, ruleInput)
              }
              // Only keep the non-fine-grained ones because the others are already covered by
              // configuredRuleInputList
              .filter { it.startsWith("//external:") }
              .distinct()
    } else {
      rule.ruleInputList.map { ruleInput: String ->
        transformRuleInput(fineGrainedHashExternalRepos, ruleInput)
      }
    }
  }

  val name: String = rule.name

  private fun transformRuleInput(
      fineGrainedHashExternalRepos: Set<String>,
      ruleInput: String
  ): String {
    if (isNotMainRepo(ruleInput) &&
        ruleInput.startsWith("@") &&
        fineGrainedHashExternalRepos.none {
          ruleInput.startsWith("@$it") || ruleInput.startsWith("@@${it}")
        }) {
      val splitRule = ruleInput.split("//".toRegex()).dropLastWhile { it.isEmpty() }.toTypedArray()
      if (splitRule.size == 2) {
        var externalRule = splitRule[0]
        externalRule = externalRule.replaceFirst("@+".toRegex(), "")
        return String.format("//external:%s", externalRule)
      }
    }
    return ruleInput
  }

  private fun isNotMainRepo(ruleInput: String): Boolean {
    return !ruleInput.startsWith("//") &&
        !ruleInput.startsWith("@//") &&
        !ruleInput.startsWith("@@//")
  }
}
