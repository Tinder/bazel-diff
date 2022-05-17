package com.bazel_diff.extensions

import java.nio.charset.StandardCharsets

fun ByteArray.toHexString(): String {
    val hexChars = ByteArray(size * 2)
    for (j in indices) {
        val v = get(j).toInt() and 0xFF
        hexChars[j * 2] = HEX_ARRAY[v ushr 4]
        hexChars[j * 2 + 1] = HEX_ARRAY[v and 0x0F]
    }
    return String(hexChars, StandardCharsets.UTF_8)
}

private val HEX_ARRAY = "0123456789abcdef".toByteArray(StandardCharsets.US_ASCII)
