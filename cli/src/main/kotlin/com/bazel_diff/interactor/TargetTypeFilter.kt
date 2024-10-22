package com.bazel_diff.interactor

import com.bazel_diff.hash.TargetHash

class TargetTypeFilter(
    private val targetTypes: Set<String>?,
    private val targets: Map<String, TargetHash>
) {

  fun accepts(label: String): Boolean {
    if (targetTypes == null) {
      return true
    }
    val targetHash = targets[label]!!
    if (!targetHash.hasType()) {
      throw IllegalStateException(
          "No target type info found, please re-generate the target hashes JSON with --includeTypeTarget!")
    }
    return targetTypes.contains(targetHash.type)
  }
}
