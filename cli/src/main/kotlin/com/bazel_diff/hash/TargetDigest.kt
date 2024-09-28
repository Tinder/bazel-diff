package com.bazel_diff.hash
// import com.google.common.hash.Hasher
import com.google.common.hash.Hashing

data class TargetDigest(
    val overallDigest: ByteArray,
    val directDigest: ByteArray,
) {
    fun clone(): TargetDigest {
        return TargetDigest(overallDigest.clone(), directDigest.clone())
    }
}

fun targetSha256(block: TargetDigestBuilder.() -> Unit): TargetDigest {
    val hasher = TargetDigestBuilder()
    hasher.apply(block)
    return hasher.finish()
}

class TargetDigestBuilder {
    private val overallHasher = Hashing.sha256().newHasher()
    private val directHasher = Hashing.sha256().newHasher()

    fun putDirectBytes(block: ByteArray?) {
        block?.let { directHasher.putBytes(it) }
    }

    fun putBytes(block: ByteArray?) {
        block?.let { overallHasher.putBytes(it) }
    }

    fun finish(): TargetDigest {
        val directHash = directHasher.hash().asBytes().clone()
        overallHasher.putBytes(directHash)

        return TargetDigest(
            overallHasher.hash().asBytes().clone(),
            directHash,
        )
    }
}
