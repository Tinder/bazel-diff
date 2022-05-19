package com.bazel_diff.di

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

fun hasherModule(
    workingDirectory: Path,
    bazelPath: Path,
    startupOptions: List<String>,
    commandOptions: List<String>,
    keepGoing: Boolean?,
): Module = module {
    val debug = System.getProperty("DEBUG", "false").equals("true")
    single {
        BazelQueryService(
            workingDirectory,
            bazelPath,
            startupOptions,
            commandOptions,
            keepGoing,
            debug
        )
    }
    single { BazelClient() }
    single { BuildGraphHasher(get()) }
    single { TargetHasher() }
    single { RuleHasher() }
    single { SourceFileHasher() }
    single(named("working-directory")) { workingDirectory }
}

fun loggingModule(verbose: Boolean) = module {
    single<Logger> { StdoutLogger(verbose) }
}

fun serialisationModule() = module {
    single { GsonBuilder().setPrettyPrinting().create() }
}
