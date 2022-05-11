package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.log.Logger
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import org.koin.core.qualifier.named
import java.nio.file.Path
import java.nio.file.Paths

class SourceFileHasher : KoinComponent {
    private val workingDirectory: Path by inject(qualifier = named("working-directory"))
    private val logger: Logger by inject()

    fun digest(sourceFileTarget: BazelSourceFileTarget): ByteArray {
        return sha256 {
            val name = sourceFileTarget.name
            if (name.startsWith("//")) {
                val filenameSubstring = name.substring(2)
                val filenamePath = filenameSubstring.replaceFirst(":".toRegex(), "/")
                val absoluteFilePath = Paths.get(workingDirectory.toString(), filenamePath)
                val file = absoluteFilePath.toFile()
                if (file.exists() && file.isFile) {
                    putFile(file)
                } else {
                    logger.w { "File $absoluteFilePath not found" }
                }
                safePutBytes(sourceFileTarget.seed)
                safePutBytes(name.toByteArray())
            }
        }
    }

    fun softDigest(sourceFileTarget: BazelSourceFileTarget): ByteArray? {
        val name = sourceFileTarget.name
        if (!name.startsWith("//")) return null

        val filenameSubstring = name.substring(2)
        val filenamePath = filenameSubstring.replaceFirst(":".toRegex(), "/")
        val absoluteFilePath = Paths.get(workingDirectory.toString(), filenamePath)
        val file = absoluteFilePath.toFile()
        if (!file.exists() || !file.isFile) return null

        return digest(sourceFileTarget)
    }
}
