package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.io.ContentHashProvider
import com.bazel_diff.log.Logger
import java.nio.file.Path
import java.nio.file.Paths
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import org.koin.core.qualifier.named

interface SourceFileHasher {
  fun digest(
      sourceFileTarget: BazelSourceFileTarget,
      modifiedFilepaths: Set<Path> = emptySet()
  ): ByteArray

  fun softDigest(
      sourceFileTarget: BazelSourceFileTarget,
      modifiedFilepaths: Set<Path> = emptySet()
  ): ByteArray?
}

class SourceFileHasherImpl : KoinComponent, SourceFileHasher {
  private val workingDirectory: Path
  private val logger: Logger
  private val relativeFilenameToContentHash: Map<String, String>?
  private val fineGrainedHashExternalRepoNames: Set<String>
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
    this.fineGrainedHashExternalRepoNames =
        fineGrainedHashExternalRepos.map { it.replaceFirst("@+".toRegex(), "") }.toSet()
    val externalRepoResolver: ExternalRepoResolver by inject()
    this.externalRepoResolver = externalRepoResolver
  }

  constructor(
      workingDirectory: Path,
      relativeFilenameToContentHash: Map<String, String>?,
      externalRepoResolver: ExternalRepoResolver,
      fineGrainedHashExternalRepos: Set<String> = emptySet()
  ) {
    this.workingDirectory = workingDirectory
    this.relativeFilenameToContentHash = relativeFilenameToContentHash
    this.fineGrainedHashExternalRepoNames =
        fineGrainedHashExternalRepos.map { it.replaceFirst("@+".toRegex(), "") }.toSet()
    this.externalRepoResolver = externalRepoResolver
  }

  override fun digest(
      sourceFileTarget: BazelSourceFileTarget,
      modifiedFilepaths: Set<Path>
  ): ByteArray {
    return sha256 {
      val name = sourceFileTarget.name
      val index = isMainRepo(name)
      val filenamePath =
          if (index != -1) {
            val filenameSubstring = name.substring(index)
            Paths.get(filenameSubstring.removePrefix(":").replace(':', '/'))
          } else if (name.startsWith("@")) {
            val parts = name.replaceFirst("@+".toRegex(), "").split("//")
            if (parts.size != 2) {
              logger.w { "Invalid source label $name" }
              return@sha256
            }
            val repoName = parts[0]
            if (repoName !in fineGrainedHashExternalRepoNames) {
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
        // Mark that file exists (via content hash)
        putBytes(byteArrayOf(0x01))
      } else {
        val absoluteFilePath = workingDirectory.resolve(filenamePath)
        val file = absoluteFilePath.toFile()
        if (file.exists()) {
          if (file.isFile) {
            if (modifiedFilepaths.isEmpty()) {
              putFile(file)
            } else if (modifiedFilepaths.any { workingDirectory.resolve(it) == absoluteFilePath }) {
              putFile(file)
            }
            // Mark that file exists
            putBytes(byteArrayOf(0x01))
          }
        } else {
          logger.w { "File $absoluteFilePath not found" }
          // Mark that file is missing
          putBytes(byteArrayOf(0x00))
        }
      }
      safePutBytes(sourceFileTarget.seed)
      safePutBytes(name.toByteArray())
    }
  }

  override fun softDigest(
      sourceFileTarget: BazelSourceFileTarget,
      modifiedFilepaths: Set<Path>
  ): ByteArray? {
    val name = sourceFileTarget.name
    val index = isMainRepo(name)
    if (index == -1) return null

    val filenameSubstring = name.substring(index)
    val filenamePath = filenameSubstring.replaceFirst(":".toRegex(), "/")
    val absoluteFilePath = Paths.get(workingDirectory.toString(), filenamePath)
    val file = absoluteFilePath.toFile()
    if (!file.exists() || !file.isFile) return null

    return digest(sourceFileTarget, modifiedFilepaths)
  }

  private fun isMainRepo(name: String): Int {
    if (name.startsWith("//")) {
      return 2
    }
    if (name.startsWith("@//")) {
      return 3
    }
    if (name.startsWith("@@//")) {
      return 4
    }
    return -1
  }
}
