package com.bazel_diff.hash

data class TargetHash(
    val type: String, // Rule/GeneratedFile/SourceFile/...
    val hash: String,
    val directHash: String,
    val deps: List<String>? = null
) {
  val hashWithType by lazy { "${type}#${hash}~${directHash}" }

  val totalHash by lazy { "${hash}~${directHash}" }

  fun toJson(includeTargetType: Boolean): String {
    return if (includeTargetType) {
      hashWithType
    } else {
      totalHash
    }
  }

  fun hasType(): Boolean {
    return type.isNotEmpty()
  }

  companion object {
    fun fromJson(json: String): TargetHash {
      val parts = json.split("#")
      return when (parts.size) {
        1 -> Pair("", parts[0])
        2 -> Pair(parts[0], parts[1])
        else -> throw IllegalArgumentException("Invalid targetHash format: $json")
      }.let { (type, hash) ->
        val hashes = hash.split("~")
        when (hashes.size) {
          2 -> TargetHash(type, hashes[0], hashes[1])
          else -> throw IllegalArgumentException("Invalid targetHash format: $json")
        }
      }
    }
  }
}
