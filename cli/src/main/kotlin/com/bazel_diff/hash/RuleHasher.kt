package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.bazel.decodeConfiguredRuleInputLabel
import com.bazel_diff.log.Logger
import com.google.common.annotations.VisibleForTesting
import java.nio.file.Path
import java.util.concurrent.ConcurrentMap
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

class RuleHasher(
    private val useCquery: Boolean,
    private val trackDepLabels: Boolean,
    private val fineGrainedHashExternalRepos: Set<String>
) : KoinComponent {
  private val logger: Logger by inject()
  private val sourceFileHasher: SourceFileHasher by inject()

  @VisibleForTesting class CircularDependencyException(message: String) : Exception(message)

  private fun raiseCircularDependency(
      depPath: LinkedHashSet<String>,
      begin: String
  ): CircularDependencyException {
    val sb =
        StringBuilder().appendLine("Circular dependency detected: ").append(begin).append(" -> ")
    val circularPath = depPath.toList().takeLastWhile { it != begin }
    circularPath.forEach { sb.append(it).append(" -> ") }
    sb.append(begin)
    return CircularDependencyException(sb.toString())
  }

  fun digest(
      rule: BazelRule,
      allRulesMap: Map<String, BazelRule>,
      ruleHashes: ConcurrentMap<String, TargetDigest>,
      sourceDigests: ConcurrentMap<String, ByteArray>,
      seedHash: ByteArray?,
      packageBzlSeeds: Map<String, ByteArray>,
      depPath: LinkedHashSet<String>?,
      ignoredAttrs: Set<String>,
      modifiedFilepaths: Set<Path>
  ): TargetDigest {
    val depPathClone = if (depPath != null) LinkedHashSet(depPath) else LinkedHashSet()
    if (depPathClone.contains(rule.name)) {
      throw raiseCircularDependency(depPathClone, rule.name)
    }
    depPathClone.add(rule.name)
    ruleHashes[rule.name]?.let {
      return it
    }

    val finalHashValue =
        targetSha256(trackDepLabels) {
          putDirectBytes(rule.digest(ignoredAttrs))
          putDirectBytes(seedHash)
          // Mix in the `.bzl` seed for this rule's own package only. Each rule always looks up
          // its own package (not the caller's), so this stays consistent under the memoized,
          // depth-first recursion below and a macro edit re-hashes only the packages that
          // `load()` it (issue #365).
          putDirectBytes(packageBzlSeeds[labelToPackage(rule.name)])

          for (ruleInput in rule.ruleInputList(useCquery, fineGrainedHashExternalRepos)) {
            // Under --useCquery, `ruleInput` may carry an embedded configurationChecksum (see
            // BazelRule.CONFIGURED_RULE_INPUT_SEPARATOR / #359). The full encoded string is what
            // we mix into the hash so two configured graphs differing only by per-edge
            // configuration produce distinct digests; the bare label is what we use to look up
            // the input in `allRulesMap` / `sourceDigests` and to track in `deps`.
            putDirectBytes(ruleInput.toByteArray())
            val inputLabel = decodeConfiguredRuleInputLabel(ruleInput)

            val inputRule = allRulesMap[inputLabel]
            when {
              inputRule == null && sourceDigests.containsKey(inputLabel) -> {
                putDirectBytes(sourceDigests[inputLabel])
              }
              inputRule?.name != null && inputRule.name != rule.name -> {
                val ruleInputHash =
                    digest(
                        inputRule,
                        allRulesMap,
                        ruleHashes,
                        sourceDigests,
                        seedHash,
                        packageBzlSeeds,
                        depPathClone,
                        ignoredAttrs,
                        modifiedFilepaths)
                putTransitiveBytes(inputLabel, ruleInputHash.overallDigest)
              }
              else -> {
                val heuristicDigest =
                    sourceFileHasher.softDigest(
                        BazelSourceFileTarget(inputLabel, ByteArray(0)), modifiedFilepaths)
                when {
                  heuristicDigest != null -> {
                    logger.i {
                      "Source file $inputLabel picked up as an input for rule ${rule.name}"
                    }
                    sourceDigests[inputLabel] = heuristicDigest
                    putDirectBytes(heuristicDigest)
                  }
                  else ->
                      logger.w {
                        "Unable to calculate digest for input $inputLabel for rule ${rule.name}"
                      }
                }
              }
            }
          }
        }

    return finalHashValue.also { ruleHashes[rule.name] = it }
  }
}
