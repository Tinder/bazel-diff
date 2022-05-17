package com.bazel_diff

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.hash.RuleHasher
import com.bazel_diff.hash.SourceFileHasher
import com.bazel_diff.hash.TargetHasher
import com.bazel_diff.log.Logger
import com.bazel_diff.log.StdoutLogger
import com.google.gson.GsonBuilder
import org.koin.core.module.Module
import org.koin.core.qualifier.named
import org.koin.dsl.module
import java.nio.file.Path

fun testModule(): Module = module {
    single<Logger> { SilentLogger }
    single { BazelClient() }
    single { BuildGraphHasher(get()) }
    single { TargetHasher() }
    single { RuleHasher() }
    single { SourceFileHasher() }
    single { GsonBuilder().setPrettyPrinting().create() }
}

object SilentLogger : Logger {
    override fun e(block: () -> String) = Unit
    override fun e(throwable: Throwable, block: () -> String) = Unit
    override fun w(block: () -> String) = Unit
    override fun i(block: () -> String) = Unit
}
