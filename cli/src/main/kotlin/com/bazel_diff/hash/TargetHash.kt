package com.bazel_diff.hash

data class TargetHash(
    val type: String, // Rule/GeneratedFile/SourceFile/...
    val hash: String
) {
    val hashWithType by lazy {
        "${type}#${hash}"
    }

    fun toJson(includeTargetType: Boolean): String {
        return if (includeTargetType) {
            hashWithType
        } else {
            hash
        }
    }
}