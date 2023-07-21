package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelTarget
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.util.concurrent.ConcurrentMap

class TargetHasher : KoinComponent {
    private val ruleHasher: RuleHasher by inject()

    fun digest(
        target: BazelTarget,
        allRulesMap: Map<String, BazelRule>,
        sourceDigests: ConcurrentMap<String, ByteArray>,
        ruleHashes: ConcurrentMap<Pair<String, Int?>, ByteArray>,
        seedHash: ByteArray?,
        ignoredAttrs: Set<String>,
        depth: Int?
    ): ByteArray {
        return when (target) {
            is BazelTarget.GeneratedFile -> {
                val generatingRuleDigest = ruleHashes[Pair(target.generatingRuleName, depth)]
                if (generatingRuleDigest != null) {
                    generatingRuleDigest.clone()
                } else {
                    val generatingRule = allRulesMap[target.generatingRuleName]
                        ?: throw RuntimeException("Unexpected generating rule ${target.generatingRuleName}")
                    ruleHasher.digest(
                        generatingRule,
                        allRulesMap,
                        ruleHashes,
                        sourceDigests,
                        seedHash,
                        depPath = null,
                        ignoredAttrs,
                        depth
                    )
                }
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
                    depth
                )
            }
            is BazelTarget.SourceFile -> sha256 {
                safePutBytes(sourceDigests[target.sourceFileName])
                safePutBytes(seedHash)
            }
        }
    }
}
