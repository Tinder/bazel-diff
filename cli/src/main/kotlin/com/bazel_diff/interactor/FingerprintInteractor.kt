package com.bazel_diff.interactor

import com.bazel_diff.extensions.toHexString
import com.bazel_diff.hash.safePutBytes
import com.bazel_diff.hash.sha256

/**
 * Inputs to the snapshot cache key (a.k.a. the "fingerprint"). See `docs/firecracker-snapshots.md`
 * §5.2.
 *
 * A Firecracker snapshot may only be consumed when the consuming environment matches the recording
 * environment on everything that could change the build graph `generate-hashes` produces. These are
 * exactly those inputs.
 */
data class FingerprintInputs(
    /** bazel-diff's own version (from [com.bazel_diff.cli.VersionProvider]). */
    val bazelDiffVersion: String,
    /** The `bazel version` "Build label" string of the bazel binary used. */
    val bazelVersion: String,
    /** Raw bytes of `MODULE.bazel.lock`, or null if the workspace has no lockfile. */
    val moduleLockContent: ByteArray?,
    /** Map of bazelrc path (relative to workspace) -> raw contents, including imported rc files. */
    val bazelrcContents: Map<String, ByteArray>,
    /**
     * The canonicalized flag set that affects what `generate-hashes` queries or how it hashes. Keys
     * are stable flag identifiers; values are their stringified settings. See
     * [com.bazel_diff.cli.FingerprintCommand.collectFlags].
     */
    val flags: Map<String, String>,
)

/**
 * Result of computing a fingerprint: the overall key plus per-component sub-hashes for debugging.
 */
data class FingerprintResult(
    val fingerprint: String,
    val components: Map<String, String>,
)

/**
 * Computes the snapshot cache key from [FingerprintInputs].
 *
 * Pure and deterministic: identical inputs always yield an identical fingerprint, independent of
 * map iteration order. This is the part of the Firecracker design that is fully unit-testable with
 * no VM and no Bazel server (RFC Phase 1).
 */
class FingerprintInteractor {
  fun compute(inputs: FingerprintInputs): FingerprintResult {
    val components = LinkedHashMap<String, String>()

    components["bazelDiffVersion"] =
        sha256 { safePutBytes(inputs.bazelDiffVersion.toByteArray()) }.toHexString()
    components["bazelVersion"] =
        sha256 { safePutBytes(inputs.bazelVersion.toByteArray()) }.toHexString()
    components["moduleLock"] =
        sha256 {
              // Tag presence so a missing lockfile never collides with a present-but-empty one.
              val lock = inputs.moduleLockContent
              if (lock == null) {
                putBytes("absent".toByteArray())
              } else {
                putBytes("present".toByteArray())
                putBytes(lock)
              }
            }
            .toHexString()
    components["bazelrc"] =
        sha256 {
              // Sort by path so iteration order never changes the result.
              inputs.bazelrcContents.toSortedMap().forEach { (path, content) ->
                putBytes(path.toByteArray())
                safePutBytes(content)
              }
            }
            .toHexString()
    components["flags"] =
        sha256 {
              inputs.flags.toSortedMap().forEach { (k, v) ->
                putBytes(k.toByteArray())
                putBytes(v.toByteArray())
              }
            }
            .toHexString()

    val fingerprint =
        sha256 {
              components.toSortedMap().forEach { (k, v) ->
                putBytes(k.toByteArray())
                putBytes(v.toByteArray())
              }
            }
            .toHexString()

    return FingerprintResult(fingerprint, components)
  }
}
