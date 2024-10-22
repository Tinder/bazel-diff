package com.bazel_diff.hash
// import com.google.common.hash.Hasher
import com.google.common.hash.Hashing

data class TargetDigest(
    val overallDigest: ByteArray,
    val directDigest: ByteArray,
    val deps: List<String>? = null,
) {
  fun clone(newDeps: List<String>? = null): TargetDigest {
    var toUse = newDeps
    if (newDeps == null) {
      toUse = deps
    }
    return TargetDigest(overallDigest.clone(), directDigest.clone(), toUse)
  }
}

fun targetSha256(trackDepLabels: Boolean, block: TargetDigestBuilder.() -> Unit): TargetDigest {
  val hasher = TargetDigestBuilder(trackDepLabels)
  hasher.apply(block)
  return hasher.finish()
}

class TargetDigestBuilder(trackDepLabels: Boolean) {

  private val overallHasher = Hashing.sha256().newHasher()
  private val directHasher = Hashing.sha256().newHasher()
  private val deps: MutableList<String>? = if (trackDepLabels) mutableListOf() else null

  fun putDirectBytes(block: ByteArray?) {
    block?.let { directHasher.putBytes(it) }
  }

  fun putBytes(block: ByteArray?) {
    block?.let { overallHasher.putBytes(it) }
  }

  fun putTransitiveBytes(dep: String, block: ByteArray?) {
    block?.let { overallHasher.putBytes(it) }
    if (deps != null) {
      deps.add(dep)
    }
  }

  fun finish(): TargetDigest {
    val directHash = directHasher.hash().asBytes().clone()
    overallHasher.putBytes(directHash)

    return TargetDigest(
        overallHasher.hash().asBytes().clone(),
        directHash,
        deps,
    )
  }
}
