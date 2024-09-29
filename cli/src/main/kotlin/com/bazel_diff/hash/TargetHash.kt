package com.bazel_diff.hash

data class TargetHash(
    val type: String, // Rule/GeneratedFile/SourceFile/...
    val hash: String,
    val directHash: String,
    val deps: List<String>? = null
) {
    val hashWithType by lazy {
        "${type}#${hash}#${directHash}"
    }

    val totalHash by lazy {
        "${hash}#${directHash}"
    }

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
                2 -> TargetHash("", parts[0], parts[1])
                3 -> TargetHash(parts[0], parts[1], parts[2])
                else -> throw IllegalArgumentException("Invalid JSON format")
            }
        }
    }
}
