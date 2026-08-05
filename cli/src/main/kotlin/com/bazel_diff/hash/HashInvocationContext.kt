package com.bazel_diff.hash

import java.util.concurrent.ConcurrentHashMap

/**
 * Mutable state that may be shared while one build graph is hashed.
 *
 * Hashers are application singletons and may outlive a workspace checkout. Revision-derived data
 * must therefore live here instead of on a hasher. The concurrent map preserves digest
 * deduplication while targets are hashed in parallel.
 */
class HashInvocationContext {
  private val bzlDigests = ConcurrentHashMap<String, ByteArray>()

  fun bzlDigest(path: String, calculate: () -> ByteArray?): ByteArray? {
    val digest = bzlDigests.computeIfAbsent(path) { calculate() ?: MISSING_DIGEST }
    return digest.takeUnless { it === MISSING_DIGEST }
  }

  private companion object {
    val MISSING_DIGEST = ByteArray(0)
  }
}
