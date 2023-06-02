package com.bazel_diff

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.hash.RuleHasher
import com.bazel_diff.hash.SourceFileHasher
import com.bazel_diff.hash.TargetHasher
import com.bazel_diff.io.ContentHashProvider
import com.bazel_diff.log.Logger
import com.google.gson.GsonBuilder
import org.koin.core.module.Module
import org.koin.core.qualifier.named
import org.koin.dsl.module
import java.nio.file.Paths

fun testModule(): Module = module {
    single<Logger> { SilentLogger }
    single { BazelClient(false, emptySet()) }
    single { BuildGraphHasher(get()) }
    single { TargetHasher() }
    single { RuleHasher(false, emptySet()) }
    single { SourceFileHasher() }
    single { GsonBuilder().disableHtmlEscaping().setPrettyPrinting().create() }
    single(named("working-directory")) { Paths.get("working-directory") }
    single(named("output-base")) { Paths.get("output-base") }
    single { ContentHashProvider(null) }
}

object SilentLogger : Logger {
    override fun e(block: () -> String) = Unit
    override fun e(throwable: Throwable, block: () -> String) = Unit
    override fun w(block: () -> String) = Unit
    override fun i(block: () -> String) = Unit
}
