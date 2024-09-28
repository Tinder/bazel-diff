package com.bazel_diff.di

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.hash.*
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
): Module = module {
    val cmd: MutableList<String> = ArrayList<String>().apply {
            add(bazelPath.toString())
            addAll(startupOptions)
            add("info")
            add("output_base")
    }
    val result = runBlocking {
        process(
                *cmd.toTypedArray(),
                stdout = Redirect.CAPTURE,
                workingDirectory = workingDirectory.toFile(),
                stderr = Redirect.PRINT,
                destroyForcibly = true,
        )
    }
    val outputPath = Paths.get(result.output.single())
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
    single { BuildGraphHasher(get()) }
    single { TargetHasher() }
    single { RuleHasher(useCquery, fineGrainedHashExternalRepos) }
    single<SourceFileHasher> { SourceFileHasherImpl(fineGrainedHashExternalRepos) }
    single { ExternalRepoResolver(workingDirectory, bazelPath, outputPath) }
    single(named("working-directory")) { workingDirectory }
    single(named("output-base")) { outputPath }
    single { ContentHashProvider(contentHashPath) }
}

fun loggingModule(verbose: Boolean) = module {
    single<Logger> { StderrLogger(verbose) }
}

fun serialisationModule() = module {
    single { GsonBuilder().disableHtmlEscaping().setPrettyPrinting().create() }
}
