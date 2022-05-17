package com.bazel_diff.process

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.asFlow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runInterruptible
import kotlinx.coroutines.withContext
import kotlinx.coroutines.yield
import java.io.File
import java.io.InputStream

private suspend fun <R> coroutineScopeIO(block: suspend CoroutineScope.() -> R) =
    withContext(Dispatchers.IO) {
        // Encapsulates all async calls in the current scope.
        // https://elizarov.medium.com/structured-concurrency-722d765aa952
        coroutineScope(block)
    }

@ExperimentalCoroutinesApi
@Suppress("BlockingMethodInNonBlockingContext", "LongParameterList", "ComplexMethod")
suspend fun process(
    vararg command: String,
    stdout: Redirect = Redirect.PRINT,
    stderr: Redirect = Redirect.PRINT,
    env: Map<String, String>? = null,
    workingDirectory: File? = null,
    /** Determine if process should be destroyed forcibly on job cancellation. */
    destroyForcibly: Boolean = false,
    /** Consume without delay all streams configured with [Redirect.CAPTURE]. */
    consumer: suspend (String) -> Unit = {},
): ProcessResult = coroutineScopeIO {
    // Based on the fact that it's hardcore to achieve manually:
    // https://stackoverflow.com/a/4959696
    val captureAll = stdout == stderr && stderr == Redirect.CAPTURE

    // https://www.baeldung.com/java-lang-processbuilder-api
    val process = ProcessBuilder(*command).apply {
        if (captureAll) {
            redirectErrorStream(true)
        } else {
            redirectOutput(stdout.toNative())
            redirectError(stderr.toNative())
        }

        workingDirectory?.let { directory(it) }
        env?.let { environment().putAll(it) }
    }.start()

    // Handles async consumptions before the blocking output handling.
    if (stdout is Redirect.Consume) {
        process.inputStream.lineFlow(stdout.consumer)
    }
    if (stderr is Redirect.Consume) {
        process.errorStream.lineFlow(stderr.consumer)
    }

    val output = async {
        when {
            captureAll || stdout == Redirect.CAPTURE ->
                process.inputStream
            stderr == Redirect.CAPTURE ->
                process.errorStream
            else -> null
        }?.lineFlow { f ->
            f.map {
                yield()
                it.also { consumer(it) }
            }.toList()
        } ?: emptyList()
    }

    try {
        @Suppress("UNCHECKED_CAST")
        ProcessResult(
            // Consume the output before waitFor,
            // ensuring no content is skipped.
            output = awaitAll(output).last(),
            resultCode = runInterruptible { process.waitFor() },
        )
    } catch (e: CancellationException) {
        when (destroyForcibly) {
            true -> process.destroyForcibly()
            false -> process.destroy()
        }
        throw e
    }
}

private suspend fun <T> InputStream.lineFlow(block: suspend (Flow<String>) -> T): T =
    bufferedReader().use { it.lineSequence().asFlow().let { f -> block(f) } }
