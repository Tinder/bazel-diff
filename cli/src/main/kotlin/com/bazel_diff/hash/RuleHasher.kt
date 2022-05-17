package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.log.Logger
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.util.concurrent.ConcurrentMap

class RuleHasher : KoinComponent {
    private val logger: Logger by inject()
    private val sourceFileHasher: SourceFileHasher by inject()

    fun digest(
        rule: BazelRule,
        allRulesMap: Map<String, BazelRule>,
        ruleHashes: ConcurrentMap<String, ByteArray>,
        sourceDigests: ConcurrentMap<String, ByteArray>,
        seedHash: ByteArray?
    ): ByteArray {
        ruleHashes[rule.name]?.let { return it }

        val finalHashValue = sha256 {
            safePutBytes(rule.digest)
            safePutBytes(seedHash)

            for (ruleInput in rule.ruleInputList) {
                safePutBytes(ruleInput.toByteArray())

                val inputRule = allRulesMap[ruleInput]
                when {
                    inputRule == null && sourceDigests.containsKey(ruleInput) -> {
                        safePutBytes(sourceDigests[ruleInput])
                    }
                    inputRule?.name != null && inputRule.name != rule.name -> {
                        val ruleInputHash = digest(
                            inputRule,
                            allRulesMap,
                            ruleHashes,
                            sourceDigests,
                            seedHash
                        )
                        safePutBytes(ruleInputHash)
                    }
                    else -> {
                        val heuristicDigest = sourceFileHasher.softDigest(BazelSourceFileTarget(ruleInput, ByteArray(0)))
                        when {
                            heuristicDigest != null -> {
                                logger.i { "Source file $ruleInput picked up as an input for rule ${rule.name}" }
                                sourceDigests[ruleInput] = heuristicDigest
                                safePutBytes(heuristicDigest)
                            }
                            else -> logger.w { "Unable to calculate digest for input $ruleInput for rule ${rule.name}" }
                        }
                    }
                }
            }
        }

        return finalHashValue.also { ruleHashes[rule.name] = it }
    }
}
