package com.bazel_diff.di

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.hash.RuleHasher
import com.bazel_diff.hash.SourceFileHasher
import com.bazel_diff.hash.TargetHasher
import com.bazel_diff.io.ContentHashProvider
import com.bazel_diff.log.Logger
import com.bazel_diff.log.StderrLogger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import com.google.gson.GsonBuilder
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.module.Module
import org.koin.core.qualifier.named
import org.koin.dsl.module
import java.io.File
import java.nio.file.Path
import java.nio.file.Paths

@OptIn(ExperimentalCoroutinesApi::class)
fun hasherModule(
    workingDirectory: Path,
    bazelPath: Path,
    contentHashPath: File?,
    startupOptions: List<String>,
    commandOptions: List<String>,
    cqueryOptions: List<String>,
    useCquery: Boolean,
    keepGoing: Boolean,
    fineGrainedHashExternalRepos: Set<String>,
    depth: Int?,
): Module = module {
    val debug = System.getProperty("DEBUG", "false").equals("true")
    single {
        BazelQueryService(
            workingDirectory,
            bazelPath,
            startupOptions,
            commandOptions,
            cqueryOptions,
            keepGoing,
            debug
        )
    }
    single { BazelClient(useCquery, fineGrainedHashExternalRepos) }
    single { BuildGraphHasher(get(), depth) }
    single { TargetHasher() }
    single { RuleHasher(useCquery, fineGrainedHashExternalRepos) }
    single { SourceFileHasher(fineGrainedHashExternalRepos) }
    single(named("working-directory")) { workingDirectory }
    single(named("output-base")) {
        val result = runBlocking {
            process(
                bazelPath.toString(), "info", "output_base",
                stdout = Redirect.CAPTURE,
                workingDirectory = workingDirectory.toFile(),
                stderr = Redirect.PRINT,
                destroyForcibly = true,
            )
        }
        Paths.get(result.output.single())
    }
    single { ContentHashProvider(contentHashPath) }
}

fun loggingModule(verbose: Boolean) = module {
    single<Logger> { StderrLogger(verbose) }
}

fun serialisationModule() = module {
    single { GsonBuilder().disableHtmlEscaping().setPrettyPrinting().create() }
}
