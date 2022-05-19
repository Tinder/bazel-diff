package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.io.ContentHashProvider
import com.bazel_diff.log.Logger
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import org.koin.core.qualifier.named
import java.nio.file.Path
import java.nio.file.Paths

class SourceFileHasher : KoinComponent {
    private val workingDirectory: Path
    private val logger: Logger
    private val relativeFilenameToContentHash: Map<String, String>?
    init {
        val logger: Logger by inject()
        this.logger = logger
    }

    constructor() {
        val workingDirectory: Path by inject(qualifier = named("working-directory"))
        this.workingDirectory = workingDirectory
        val contentHashProvider: ContentHashProvider by inject()
        relativeFilenameToContentHash = contentHashProvider.filenameToHash
    }

    constructor(workingDirectory: Path, relativeFilenameToContentHash: Map<String, String>?) {
        this.workingDirectory = workingDirectory
        this.relativeFilenameToContentHash = relativeFilenameToContentHash
    }

    fun digest(sourceFileTarget: BazelSourceFileTarget): ByteArray {
        return sha256 {
            val name = sourceFileTarget.name
            if (name.startsWith("//")) {
                val filenameSubstring = name.substring(2)
                val filenamePath = filenameSubstring.replaceFirst(
                        ":".toRegex(),
                        if (filenameSubstring.startsWith(":")) "" else "/"
                )
                if (relativeFilenameToContentHash?.contains(filenamePath) == true) {
                    val contentHash = relativeFilenameToContentHash.getValue(filenamePath)
                    safePutBytes(contentHash.toByteArray())
                } else {
                    val absoluteFilePath = Paths.get(workingDirectory.toString(), filenamePath)
                    val file = absoluteFilePath.toFile()
                    if (file.exists() && file.isFile) {
                        putFile(file)
                    } else {
                        logger.w { "File $absoluteFilePath not found" }
                    }
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
