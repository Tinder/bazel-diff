package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelTarget
import java.nio.file.Path
import java.util.concurrent.ConcurrentMap
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

class TargetHasher : KoinComponent {
  private val ruleHasher: RuleHasher by inject()

  fun digest(
      target: BazelTarget,
      allRulesMap: Map<String, BazelRule>,
      sourceDigests: ConcurrentMap<String, ByteArray>,
      ruleHashes: ConcurrentMap<String, TargetDigest>,
      seedHash: ByteArray?,
      ignoredAttrs: Set<String>,
      modifiedFilepaths: Set<Path>
  ): TargetDigest {
    return when (target) {
      is BazelTarget.GeneratedFile -> {
        val generatingRuleDigest = ruleHashes[target.generatingRuleName]
        var digest: TargetDigest
        if (generatingRuleDigest != null) {
          digest = generatingRuleDigest
        } else {
          val generatingRule =
              allRulesMap[target.generatingRuleName]
                  ?: throw RuntimeException(
                      "Unexpected generating rule ${target.generatingRuleName}")
          digest =
              ruleHasher.digest(
                  generatingRule,
                  allRulesMap,
                  ruleHashes,
                  sourceDigests,
                  seedHash,
                  depPath = null,
                  ignoredAttrs,
                  modifiedFilepaths)
        }

        // Add the generating rule name as a dep of the generated file.
        digest = digest.clone(newDeps = listOf(target.generatingRuleName))
        digest
      }
      is BazelTarget.Rule -> {
        ruleHasher.digest(
            target.rule,
            allRulesMap,
            ruleHashes,
            sourceDigests,
            seedHash,
            depPath = null,
            ignoredAttrs,
            modifiedFilepaths)
      }
      is BazelTarget.SourceFile -> {
        val digest = sha256 {
          safePutBytes(sourceDigests[target.sourceFileName])
          safePutBytes(seedHash)
        }
        TargetDigest(digest, digest)
      }
    }
  }
}
