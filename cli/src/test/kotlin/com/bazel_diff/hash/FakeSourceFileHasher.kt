package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelSourceFileTarget
import java.nio.file.Path
import java.util.concurrent.atomic.AtomicInteger

class FakeSourceFileHasher : SourceFileHasher {
  var fakeDigests: MutableMap<String, ByteArray> = mutableMapOf()
  var softDigestValue: ByteArray? = "fake-soft-digest".toByteArray()
  val softDigestCalls = AtomicInteger()

  override fun digest(
      sourceFileTarget: BazelSourceFileTarget,
      modifiedFilepaths: Set<Path>
  ): ByteArray {
    return fakeDigests[sourceFileTarget.name]
        ?: throw IllegalArgumentException("Digest not found for ${sourceFileTarget.name}")
  }

  override fun softDigest(
      sourceFileTarget: BazelSourceFileTarget,
      modifiedFilepaths: Set<Path>
  ): ByteArray? {
    softDigestCalls.incrementAndGet()
    return softDigestValue
  }

  fun add(name: String, digest: ByteArray) {
    fakeDigests[name] = digest
  }
}
