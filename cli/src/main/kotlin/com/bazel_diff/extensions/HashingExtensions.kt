package com.bazel_diff.hash

import com.bazel_diff.extensions.compatClear
import com.bazel_diff.extensions.compatFlip
import com.bazel_diff.io.ByteBufferPool
import com.google.common.hash.Hasher
import com.google.common.hash.Hashing
import java.io.BufferedInputStream
import java.io.File
import java.io.FileInputStream

fun sha256(block: Hasher.() -> Unit): ByteArray {
  val hasher = Hashing.sha256().newHasher()
  hasher.apply(block)
  return hasher.hash().asBytes().clone()
}

fun Hasher.safePutBytes(block: ByteArray?) = block?.let { putBytes(it) }

fun Hasher.putFile(file: File) {
  BufferedInputStream(FileInputStream(file.absolutePath.toString())).use { stream ->
    val buffer = pool.borrow()
    val array = buffer!!.array() // Available for non-direct buffers
    while (true) {
      var length: Int
      if (stream.read(array).also { length = it } == -1) break
      buffer.compatFlip()
      putBytes(array, 0, length)
      buffer.compatClear()
    }
    pool.recycle(buffer)
  }
}

private val pool = ByteBufferPool(1024, 10240) // 10kb
