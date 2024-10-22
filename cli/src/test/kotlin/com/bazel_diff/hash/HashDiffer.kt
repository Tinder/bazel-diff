package com.bazel_diff.hash

import assertk.assertThat
import assertk.assertions.*

/**
 * Utility class for performing hash comparisons between two maps of target hashes.
 *
 * Allows the creation of test assertions that don't depend on the underlying hash values, but
 * instead assert properties based on expected changes to hash values when comparing the results of
 * hashing two build graphs.
 *
 * @property from The map of target hashes to compare from.
 * @property to The map of target hashes to compare to.
 */
class HashDiffer(
    private val from: Map<String, TargetHash>,
    private val to: Map<String, TargetHash>
) {

  fun assertThat(ruleName: String): SingleTarget {
    return SingleTarget(from[ruleName], to[ruleName])
  }

  inner class SingleTarget(private val fromHash: TargetHash?, private val toHash: TargetHash?) {

    fun hash(): HashAssertion {
      return HashAssertion(fromHash?.hash, toHash?.hash)
    }

    fun directHash(): HashAssertion {
      return HashAssertion(fromHash?.directHash, toHash?.directHash)
    }
  }

  inner class HashAssertion(private val fromHash: String?, private val toHash: String?) {
    fun changed() {
      assertThat(fromHash).isNotEqualTo(toHash)
    }

    fun didNotChange() {
      assertThat(fromHash).isEqualTo(toHash)
    }
  }
}
