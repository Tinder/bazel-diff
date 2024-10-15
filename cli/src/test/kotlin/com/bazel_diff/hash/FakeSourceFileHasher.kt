package com.bazel_diff.hash

import java.nio.file.Path
import com.bazel_diff.bazel.BazelSourceFileTarget

class FakeSourceFileHasher : SourceFileHasher {
    var fakeDigests: MutableMap<String, ByteArray> = mutableMapOf()
    override fun digest(sourceFileTarget: BazelSourceFileTarget, modifiedFilepaths: Set<Path> ): ByteArray {
        return fakeDigests[sourceFileTarget.name] ?: throw IllegalArgumentException("Digest not found for ${sourceFileTarget.name}")
    }
    override fun softDigest(sourceFileTarget: BazelSourceFileTarget, modifiedFilepaths: Set<Path> ): ByteArray? {
        return "fake-soft-digest".toByteArray()
    }

    fun add(name: String, digest: ByteArray) {
        fakeDigests[name] = digest
    }
}
