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

// Separator used to fold a cquery dep-edge's `configurationChecksum` into a rule-input string.
// Bazel target/package names are restricted to `[A-Za-z0-9/._+=,@~-]*` plus `:` between package
// and target, so `|` is invalid in a label and is a safe in-band encoding that survives the
// existing `List<String>` return type of `ruleInputList()`. `RuleHasher` splits on this character
// to recover the bare label for `allRulesMap` / `sourceDigests` lookups and dep tracking; the
// full encoded string is what gets mixed into the transitive hash, so two configured graphs that
// share dep labels but differ in per-edge configuration produce distinct rule digests. See #359.
const val CONFIGURED_RULE_INPUT_SEPARATOR: Char = '|'

fun encodeConfiguredRuleInput(input: Build.ConfiguredRuleInput): String {
  // Fall back to a bare label when the checksum is empty -- this matches the pre-#359 behaviour
  // for the known Bazel quirk where cquery's `transitions=lite` output sometimes omits the
  // configurationChecksum for an edge, and avoids appending a dangling separator.
  val checksum = input.configurationChecksum
  return if (checksum.isNullOrEmpty()) {
    input.label
  } else {
    "${input.label}${CONFIGURED_RULE_INPUT_SEPARATOR}${checksum}"
  }
}

fun decodeConfiguredRuleInputLabel(encoded: String): String {
  // No separator present -> the input is a bare label (non-cquery callers, empty-checksum
  // fallback, or //external:* synthetic inputs), so this is a no-op.
  return encoded.substringBefore(CONFIGURED_RULE_INPUT_SEPARATOR)
}

class BazelRule(private val rule: Build.Rule) {
  fun digest(ignoredAttrs: Set<String>): ByteArray {
    return sha256 {
      safePutBytes(rule.ruleClassBytes.toByteArray())
      safePutBytes(rule.nameBytes.toByteArray())
      safePutBytes(rule.skylarkEnvironmentHashCodeBytes.toByteArray())
      // Hash attributes in a canonical (name-sorted) order so a rule's digest is invariant to the
      // order Bazel happens to emit them in. Attribute names are unique within a rule (the proto
      // is generated from the rule's attribute map), so a name sort fully canonicalizes the set.
      // Mirrors target-determinator's `sortedAttributesForHashing` (commit d4b6125).
      for (attribute in rule.attributeList.sortedBy { it.name }) {
        if (!DEFAULT_IGNORED_ATTRS.contains(attribute.name) &&
            !ignoredAttrs.contains(attribute.name))
            safePutBytes(attribute.toByteArray())
      }
    }
  }

  fun ruleInputList(useCquery: Boolean, fineGrainedHashExternalRepos: Set<String>): List<String> {
    // Transformed //external:* synthetic inputs so that targets depending on external repos pick
    // up hash changes from //external:* synthetic targets (e.g. from bzlmod mod show_repo or
    // WORKSPACE //external:all-targets).
    val externalSyntheticInputs =
        rule.ruleInputList
            .map { ruleInput: String ->
              transformRuleInput(fineGrainedHashExternalRepos, ruleInput)
            }
            .filter { it.startsWith("//external:") }

    val inputs =
        if (useCquery) {
          // configuredRuleInputList already covers the non-fine-grained deps (with per-edge
          // configuration folded in), so only the //external:* synthetic inputs are added.
          rule.configuredRuleInputList.map { encodeConfiguredRuleInput(it) } +
              externalSyntheticInputs
        } else {
          rule.ruleInputList + externalSyntheticInputs
        }

    // Canonicalize: dedupe and sort so a target's digest is invariant to the order Bazel happens
    // to emit (configured) rule inputs in. This matters most under --useCquery, where the same dep
    // label can surface across multiple configurations and cquery does not guarantee a stable
    // emission order -- without canonicalization an unchanged target could hash differently
    // between two otherwise-identical graphs. RuleHasher mixes these bytes into the digest in list
    // order, so a deterministic order is what keeps the hash stable. Mirrors
    // target-determinator's `canonicalizeRuleInputs` (commit d4b6125).
    return inputs.distinct().sorted()
  }

  val name: String = rule.name

  private fun transformRuleInput(
      fineGrainedHashExternalRepos: Set<String>,
      ruleInput: String
  ): String {
    if (isNotMainRepo(ruleInput) &&
        ruleInput.startsWith("@") &&
        fineGrainedHashExternalRepos.none { ruleInput.startsWith(it) }) {
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
