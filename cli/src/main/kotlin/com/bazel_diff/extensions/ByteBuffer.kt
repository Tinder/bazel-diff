package com.bazel_diff.extensions

import java.nio.Buffer
import java.nio.ByteBuffer

/** Fix for NoSuchMethodError for JRE8 */
fun ByteBuffer.compatClear() = ((this as Buffer).clear() as ByteBuffer)

fun ByteBuffer.compatFlip() = ((this as Buffer).flip() as ByteBuffer)
