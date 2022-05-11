package com.bazel_diff.process

data class ProcessResult(
    val resultCode: Int,
    val output: List<String>,
)
