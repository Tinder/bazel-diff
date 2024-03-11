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
    private val fineGrainedHashExternalRepos: Set<String>
    private val externalRepoResolver: ExternalRepoResolver

    init {
        val logger: Logger by inject()
        this.logger = logger
    }

    constructor(fineGrainedHashExternalRepos: Set<String> = emptySet()) {
        val workingDirectory: Path by inject(qualifier = named("working-directory"))
        this.workingDirectory = workingDirectory
        val contentHashProvider: ContentHashProvider by inject()
        relativeFilenameToContentHash = contentHashProvider.filenameToHash
        this.fineGrainedHashExternalRepos = fineGrainedHashExternalRepos
        val externalRepoResolver: ExternalRepoResolver by inject()
        this.externalRepoResolver = externalRepoResolver
    }

    constructor(workingDirectory: Path, relativeFilenameToContentHash: Map<String, String>?, externalRepoResolver: ExternalRepoResolver, fineGrainedHashExternalRepos: Set<String> = emptySet()) {
        this.workingDirectory = workingDirectory
        this.relativeFilenameToContentHash = relativeFilenameToContentHash
        this.fineGrainedHashExternalRepos = fineGrainedHashExternalRepos
        this.externalRepoResolver = externalRepoResolver
    }

    fun digest(sourceFileTarget: BazelSourceFileTarget): ByteArray {
        return sha256 {
            val name = sourceFileTarget.name
            val filenamePath = if (name.startsWith("//")) {
                val filenameSubstring = name.substring(2)
                Paths.get(filenameSubstring.removePrefix(":").replace(':', '/'))
            } else if (name.startsWith("@")) {
                val parts = name.substring(1).split("//")
                if (parts.size != 2) {
                    logger.w { "Invalid source label $name" }
                    return@sha256
                }
                val repoName = parts[0]
                if (repoName !in fineGrainedHashExternalRepos) {
                    return@sha256
                }
                val relativePath = Paths.get(parts[1].removePrefix(":").replace(':', '/'))
                val externalRepoRoot = externalRepoResolver.resolveExternalRepoRoot(repoName)
                externalRepoRoot.resolve(relativePath)
            } else {
                return@sha256
            }
            val filenamePathString = filenamePath.toString()
            if (relativeFilenameToContentHash?.contains(filenamePathString) == true) {
                val contentHash = relativeFilenameToContentHash.getValue(filenamePathString)
                safePutBytes(contentHash.toByteArray())
            } else {
                val absoluteFilePath = workingDirectory.resolve(filenamePath)
                val file = absoluteFilePath.toFile()
                if (file.exists()) {
                    if (file.isFile) {
                        putFile(file)
                    }
                } else {
                    logger.w { "File $absoluteFilePath not found" }
                }
            }
            safePutBytes(sourceFileTarget.seed)
            safePutBytes(name.toByteArray())
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
