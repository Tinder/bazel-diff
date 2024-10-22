package com.bazel_diff.log

interface Logger {
  fun e(block: () -> String)

  fun w(block: () -> String)

  fun i(block: () -> String)

  fun e(throwable: Throwable, block: () -> String)
}

class StderrLogger(private val verbose: Boolean) : Logger {
  override fun e(block: () -> String) {
    System.err.println("[Error] ${block.invoke()}")
  }

  override fun e(throwable: Throwable, block: () -> String) {
    System.err.println("[Error] ${block.invoke()}")
    throwable.printStackTrace()
  }

  override fun w(block: () -> String) {
    if (verbose) {
      System.err.println("[Warning] ${block.invoke()}")
    }
  }

  override fun i(block: () -> String) {
    if (verbose) {
      System.err.println("[Info] ${block.invoke()}")
    }
  }
}
