package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.io.ContentHashProvider
import com.bazel_diff.log.Logger
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.nio.file.attribute.PosixFilePermission
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
      // `filenamePath` locates the file on disk (absolute for external repos, since they live under
      // the Bazel output base). `contentHashKey` is the key used to look the file up in a
      // user-provided content-hash map and MUST stay workspace-relative / machine-independent so the
      // map is portable across machines — the resolved external repo root is an absolute path under
      // the output base (which embeds machine-specific components like the build-agent dir), so it
      // must never be used as the lookup key.
      val filenamePath: Path
      val contentHashKey: String
      if (index != -1) {
        val filenameSubstring = name.substring(index)
        filenamePath = Paths.get(filenameSubstring.removePrefix(":").replace(':', '/'))
        contentHashKey = filenamePath.toString()
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
        filenamePath = externalRepoRoot.resolve(relativePath)
        contentHashKey = Paths.get("external", repoName).resolve(relativePath).toString()
      } else {
        return@sha256
      }
      if (relativeFilenameToContentHash?.contains(contentHashKey) == true) {
        val contentHash = relativeFilenameToContentHash.getValue(contentHashKey)
        safePutBytes(contentHash.toByteArray())
        putBytes(byteArrayOf(0x01))
        val absoluteFilePath = workingDirectory.resolve(filenamePath)
        putBytes(byteArrayOf(if (isOwnerExecutable(absoluteFilePath)) 0x01 else 0x00))
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
            putBytes(byteArrayOf(if (isOwnerExecutable(absoluteFilePath)) 0x01 else 0x00))
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

  // Git's index only tracks the owner execute bit (100644 vs 100755); group/others bits don't
  // affect the build and differ between checkouts under different umasks. Mirror that behavior so
  // hashes don't churn when only group/others bits flip.
  private fun isOwnerExecutable(absoluteFilePath: Path): Boolean =
      try {
        Files.getPosixFilePermissions(absoluteFilePath).contains(PosixFilePermission.OWNER_EXECUTE)
      } catch (_: Exception) {
        absoluteFilePath.toFile().canExecute()
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
