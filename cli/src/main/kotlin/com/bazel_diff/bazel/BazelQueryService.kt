package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import com.google.devtools.build.lib.analysis.AnalysisProtosV2
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import java.io.File
import java.nio.file.Files
import java.nio.file.Path
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

private val versionComparator =
    compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }

class BazelQueryService(
    private val workingDirectory: Path,
    private val bazelPath: Path,
    private val startupOptions: List<String>,
    private val commandOptions: List<String>,
    private val cqueryOptions: List<String>,
    private val keepGoing: Boolean,
    private val noBazelrc: Boolean,
) : KoinComponent {
  private val logger: Logger by inject()
  private val modService: BazelModService by inject()
  private val version: Triple<Int, Int, Int> by lazy { runBlocking { determineBazelVersion() } }

  @OptIn(ExperimentalCoroutinesApi::class)
  private suspend fun determineBazelVersion(): Triple<Int, Int, Int> {
    val cmd = arrayOf(bazelPath.toString(), "version")
    logger.i { "Executing Bazel version command: ${cmd.joinToString()}" }
    val result =
        process(
            *cmd,
            stdout = Redirect.CAPTURE,
            workingDirectory = workingDirectory.toFile(),
            stderr = Redirect.PRINT,
            destroyForcibly = true,
        )

    if (result.resultCode != 0) {
      throw RuntimeException("Bazel version command failed, exit code ${result.resultCode}")
    }

    // "bazel version" outputs "Build label: X.Y.Z" on one of the lines; accept that or legacy "bazel X.Y.Z".
    val versionString =
        result.output
            .firstOrNull { it.startsWith("Build label: ") }
            ?.removePrefix("Build label: ")?.trim()
            ?: result.output
                .firstOrNull { it.startsWith("bazel ") }
                ?.removePrefix("bazel ")?.trim()
            ?: throw RuntimeException(
                "Bazel version command returned unexpected output: ${result.output}")
    // Trim off any prerelease suffixes (e.g., 8.6.0-rc1 or 8.6.0rc1).
    val version =
        versionString.split('-')[0].split('.').map { it.takeWhile { c -> c.isDigit() }.toInt() }.toTypedArray()
    return Triple(version[0], version[1], version[2])
  }

  // Use streamed_proto output for cquery if available. This is more efficient than the proto
  // output.
  // https://github.com/bazelbuild/bazel/commit/607d0f7335f95aa0ee236ba3c18ce2a232370cdb
  private val canUseStreamedProtoWithCquery
    get() = versionComparator.compare(version, Triple(7, 0, 0)) >= 0

  // Use an output file for (c)query if supported. This avoids excessively large stdout, which is
  // sent out on the BES.
  // https://github.com/bazelbuild/bazel/commit/514e9052f2c603c53126fbd9436bdd3ad3a1b0c7
  private val canUseOutputFile
    get() = versionComparator.compare(version, Triple(8, 2, 0)) >= 0

  // Bazel 8.6.0+ / 9.0.1+ supports `bazel mod show_repo --output=streamed_proto`, which
  // outputs Build.Repository protos for bzlmod-managed external repos.
  // https://github.com/bazelbuild/bazel/pull/28010
  val canUseBzlmodShowRepo
    get() =
        versionComparator.compare(version, Triple(8, 6, 0)) >= 0 &&
            // 9.0.0 does not have the feature; it landed in 9.0.1.
            version != Triple(9, 0, 0)

  suspend fun query(query: String, useCquery: Boolean = false): List<BazelTarget> {
    // Unfortunately, there is still no direct way to tell if a target is compatible or not with the
    // proto output
    // by itself. So we do an extra cquery with the trick at
    // https://bazel.build/extending/platforms#cquery-incompatible-target-detection to first find
    // all compatible
    // targets.
    val compatibleTargetSet =
        if (useCquery) {
          runQuery(query, useCquery = true, outputCompatibleTargets = true).useLines {
            it.filter { it.isNotBlank() }.toSet()
          }
        } else {
          emptySet()
        }
    val outputFile = runQuery(query, useCquery)

    val targets =
        outputFile.inputStream().buffered().use { proto ->
          if (useCquery) {
            if (canUseStreamedProtoWithCquery) {
                  mutableListOf<AnalysisProtosV2.CqueryResult>()
                      .apply {
                        while (true) {
                          val result =
                              AnalysisProtosV2.CqueryResult.parseDelimitedFrom(proto) ?: break
                          // EOF
                          add(result)
                        }
                      }
                      .flatMap { it.resultsList }
                } else {
                  AnalysisProtosV2.CqueryResult.parseFrom(proto).resultsList
                }
                .mapNotNull { toBazelTarget(it.target) }
                .filter { it.name in compatibleTargetSet }
          } else {
            mutableListOf<Build.Target>()
                .apply {
                  while (true) {
                    val target = Build.Target.parseDelimitedFrom(proto) ?: break
                    // EOF
                    add(target)
                  }
                }
                .mapNotNull { toBazelTarget(it) }
          }
        }

    return targets
  }

  @OptIn(ExperimentalCoroutinesApi::class)
  private suspend fun runQuery(
      query: String,
      useCquery: Boolean,
      outputCompatibleTargets: Boolean = false
  ): File {
    val queryFile = Files.createTempFile(null, ".txt").toFile()
    queryFile.deleteOnExit()
    val outputFile = Files.createTempFile(null, ".bin").toFile()
    outputFile.deleteOnExit()

    queryFile.writeText(query)

    val allowedExitCodes = mutableListOf(0)

    val cmd: MutableList<String> =
        ArrayList<String>().apply {
          add(bazelPath.toString())
          if (noBazelrc) {
            add("--bazelrc=/dev/null")
          }
          addAll(startupOptions)
          if (useCquery) {
            add("cquery")
            if (!outputCompatibleTargets) {
              // There is no need to query the transitions when querying for compatible targets.
              add("--transitions=lite")
            }
          } else {
            add("query")
          }
          add("--output")
          if (useCquery) {
            if (outputCompatibleTargets) {
              add("starlark")
              add("--starlark:file")
              val cqueryStarlarkFile = Files.createTempFile(null, ".cquery").toFile()
              cqueryStarlarkFile.deleteOnExit()
              cqueryStarlarkFile.writeText(
                  """
                    def format(target):
                        if providers(target) == None:
                            return ""
                        if "IncompatiblePlatformProvider" not in providers(target):
                            target_repr = repr(target)
                            if "<alias target" in target_repr:
                                return target_repr.split(" ")[2]
                            return str(target.label)
                        return ""
                    """
                      .trimIndent())
              add(cqueryStarlarkFile.toString())
            } else {
              add(if (canUseStreamedProtoWithCquery) "streamed_proto" else "proto")
            }
          } else {
            add("streamed_proto")
          }
          if (!useCquery) {
            add("--order_output=no")
          }
          if (keepGoing) {
            add("--keep_going")
            allowedExitCodes.add(3)
          }
          if (useCquery) {
            addAll(cqueryOptions)
            add("--consistent_labels")
          } else {
            addAll(commandOptions)
          }
          add("--query_file")
          add(queryFile.toString())
          if (canUseOutputFile) {
            add("--output_file")
            add(outputFile.toString())
          }
        }

    logger.i { "Executing Query: $query" }
    logger.i { "Command: ${cmd.toTypedArray().joinToString()}" }
    val result =
        process(
            *cmd.toTypedArray(),
            stdout = if (canUseOutputFile) Redirect.SILENT else Redirect.ToFile(outputFile),
            workingDirectory = workingDirectory.toFile(),
            stderr = Redirect.PRINT,
            destroyForcibly = true,
        )

    if (!allowedExitCodes.contains(result.resultCode)) {
      logger.w { "Bazel query failed, output: ${result.output.joinToString("\n")}" }
      throw RuntimeException(
          "Bazel query failed, exit code ${result.resultCode}, allowed exit codes: ${allowedExitCodes.joinToString()}")
    }
    return outputFile
  }

  /**
   * Queries bzlmod-managed external repo definitions using `bazel mod show_repo`.
   * Requires Bazel 8.6.0+ or 9.0.1+ which supports `--output=streamed_proto` for this command.
   *
   * The approach:
   * 1. Run `bazel mod dump_repo_mapping ""` to discover the root module's apparent→canonical
   *    repo name mapping (e.g., "bazel_diff_maven" → "rules_jvm_external++maven+maven").
   * 2. Run `bazel mod show_repo @@<canonical>... --output=streamed_proto` to get Repository
   *    proto definitions for each repo (works for both module repos and extension-generated repos).
   * 3. Create synthetic `//external:<apparent_name>` targets for each repo. This matches how
   *    `transformRuleInput` in BazelRule.kt collapses `@apparent_name//...` deps to
   *    `//external:apparent_name`, so the hashing pipeline can detect changes.
   */
  @OptIn(ExperimentalCoroutinesApi::class)
  suspend fun queryBzlmodRepos(): List<BazelTarget> {
    check(canUseBzlmodShowRepo) { "queryBzlmodRepos requires Bazel 8.6.0+ or 9.0.1+" }

    // Step 1: Get the root module's apparent → canonical repo mapping.
    val repoMapping = discoverRepoMapping()
    if (repoMapping.isEmpty()) {
      logger.w { "No repo mappings discovered, skipping mod show_repo" }
      return emptyList()
    }
    logger.i { "Discovered ${repoMapping.size} repo mappings" }

    // Build reverse map: canonical → list of apparent names
    val canonicalToApparent = mutableMapOf<String, MutableList<String>>()
    for ((apparent, canonical) in repoMapping) {
      canonicalToApparent.getOrPut(canonical) { mutableListOf() }.add(apparent)
    }

    // Step 2: Fetch repo definitions via `mod show_repo @@<canonical>... --output=streamed_proto`.
    val canonicalNames = canonicalToApparent.keys.map { "@@$it" }
    val outputFile = Files.createTempFile(null, ".bin").toFile()
    outputFile.deleteOnExit()

    val cmd: MutableList<String> =
        ArrayList<String>().apply {
          add(bazelPath.toString())
          if (noBazelrc) {
            add("--bazelrc=/dev/null")
          }
          addAll(startupOptions)
          add("mod")
          add("show_repo")
          addAll(canonicalNames)
          add("--output=streamed_proto")
        }

    logger.i { "Querying bzlmod repos: ${cmd.joinToString()}" }
    val result =
        process(
            *cmd.toTypedArray(),
            stdout = Redirect.ToFile(outputFile),
            workingDirectory = workingDirectory.toFile(),
            stderr = Redirect.PRINT,
            destroyForcibly = true,
        )

    if (result.resultCode != 0) {
      logger.w { "bazel mod show_repo failed (exit code ${result.resultCode}), skipping bzlmod repos" }
      return emptyList()
    }

    // Step 3: Parse Build.Repository messages and create synthetic targets for each apparent name.
    val repos =
        outputFile.inputStream().buffered().use { proto ->
          mutableListOf<Build.Repository>().apply {
            while (true) {
              val repo = Build.Repository.parseDelimitedFrom(proto) ?: break
              add(repo)
            }
          }
        }

    // Discover the bzlmod module-graph edges so we can encode the dep relationships between
    // synthetic //external:* targets. Without this, a target that depends on @outer//... only
    // sees //external:outer's *metadata* hash and never picks up content changes in @outer's
    // own bzlmod deps (e.g. @inner). With these edges in place, RuleHasher follows the chain
    // //:consumer -> //external:outer -> //external:inner during digest computation, so a
    // change inside @inner propagates all the way to the main-repo consumer without the user
    // having to enumerate every wrapping repo in --fineGrainedHashExternalRepos. See
    // https://github.com/Tinder/bazel-diff/issues/184 (transitive build-time chain) and
    // https://github.com/Tinder/bazel-diff/issues/197 (alias-wrap chain).
    val moduleGraphJson = modService.getModuleGraphJson()
    val moduleDepEdges =
        if (moduleGraphJson != null) {
          ModuleGraphParser().parseModuleGraphDepEdges(moduleGraphJson)
        } else {
          emptyMap()
        }
    // `bazel mod show_repo` does not populate Repository.module_key in current Bazel, so
    // bridge from a module's `name` (always present in `bazel mod graph` output) to that
    // repo's `canonical_name` by stripping any trailing `+<version>` suffix produced by
    // bzlmod's canonical-name scheme. This is best-effort: it works for the no-version-conflict
    // case (canonical = "<name>+" or "<name>+<version>"). Module-extension repos do not appear
    // in `bazel mod graph` at all, so they get no synthetic dep edges -- their contents are
    // captured via repo metadata + the per-repo content hash below.
    val moduleNameToCanonical = mutableMapOf<String, String>()
    for (repo in repos) {
      val canonical = repo.canonicalName
      val moduleName = canonical.substringBefore('+').ifEmpty { canonical }
      // Only register a name -> canonical edge if the canonical "looks like a module repo"
      // (single `+`, no extension separator). Skip extension-generated repos like
      // "rules_jvm_external++maven+maven".
      if (canonical.count { it == '+' } == 1) {
        moduleNameToCanonical[moduleName] = canonical
      }
    }
    val canonicalToRootApparent: Map<String, List<String>> =
        canonicalToApparent.mapValues { it.value.toList() }

    val targets = mutableListOf<BazelTarget.Rule>()
    for (repo in repos) {
      // Derive this repo's bzlmod module name from its canonical name and look up its direct
      // deps in the module graph. Translate each dep's module name -> its canonical name ->
      // root-visible apparent name; that's what `BazelRule.transformRuleInput` collapses
      // non-fine-grained `@<apparent>//...` rule_inputs to, so adding `//external:<apparent>`
      // as a rule_input here is what wires up the dep chain.
      val moduleName =
          repo.canonicalName.takeIf { it.count { c -> c == '+' } == 1 }?.substringBefore('+')
      val depApparentNames =
          if (moduleName != null) {
            moduleDepEdges[moduleName]
                .orEmpty()
                .mapNotNull { moduleNameToCanonical[it] }
                .flatMap { canonicalToRootApparent[it].orEmpty() }
          } else {
            emptyList()
          }
      val apparentNames = canonicalToApparent[repo.canonicalName]
      if (apparentNames != null) {
        for (apparentName in apparentNames) {
          targets.add(repositoryToTarget(repo, apparentName, depApparentNames))
        }
      } else {
        // Fallback: use canonical name if no apparent name mapping exists
        targets.add(repositoryToTarget(repo, repo.canonicalName, depApparentNames))
      }
    }

    logger.i { "Parsed ${repos.size} bzlmod repos → ${targets.size} synthetic targets" }
    return targets
  }

  /**
   * Converts a Build.Repository proto into a synthetic BazelTarget.Rule named
   * `//external:<targetName>`. This mirrors how WORKSPACE repos appear as `//external:*`
   * targets, and matches the names produced by `transformRuleInput` in BazelRule.kt.
   *
   * For each bzlmod dep of this repo (as discovered from `bazel mod graph`) a corresponding
   * `//external:<dep_apparent_name>` is added to the rule's `rule_input` list, so
   * [RuleHasher] follows the dep chain when computing the digest. For repos backed by a
   * `local_repository` rule (which is what `local_path_override` lowers to), the contents
   * of the local directory are also rolled into a synthetic `_bazel_diff_content_hash`
   * attribute so file content changes inside the repo flip the synthetic target's hash.
   */
  private fun repositoryToTarget(
      repo: Build.Repository,
      targetName: String,
      depApparentNames: List<String>
  ): BazelTarget.Rule {
    val ruleClass = repo.repoRuleName.ifEmpty { "bzlmod_repo" }

    val attributes = repo.attributeList.toMutableList()
    val contentHash = computeLocalRepoContentHash(repo)
    if (contentHash != null) {
      attributes.add(
          Build.Attribute.newBuilder()
              .setName("_bazel_diff_content_hash")
              .setType(Build.Attribute.Discriminator.STRING)
              .setStringValue(contentHash)
              .build())
    }

    val ruleBuilder =
        Build.Rule.newBuilder()
            .setName("//external:$targetName")
            .setRuleClass(ruleClass)
            .addAllAttribute(attributes)
    for (dep in depApparentNames.toSortedSet()) {
      if (dep != targetName) ruleBuilder.addRuleInput("//external:$dep")
    }

    val target =
        Build.Target.newBuilder()
            .setType(Build.Target.Discriminator.RULE)
            .setRule(ruleBuilder)
            .build()
    return BazelTarget.Rule(target)
  }

  /**
   * Returns a stable hex sha256 over the files inside a `local_repository`-backed repo on
   * disk, or null if the repo is not local-backed or the directory cannot be read.
   *
   * `local_path_override(module_name = "X", path = "...")` in MODULE.bazel lowers to a
   * `local_repository` rule, whose `path` attribute is relative to the workspace root. Hashing
   * that directory makes file content edits surface in the synthetic //external:X target's
   * digest, which fixes the "external repo file change is invisible" half of
   * [#184](https://github.com/Tinder/bazel-diff/issues/184) /
   * [#197](https://github.com/Tinder/bazel-diff/issues/197).
   */
  private fun computeLocalRepoContentHash(repo: Build.Repository): String? {
    if (repo.repoRuleName != "local_repository") return null
    val pathAttr =
        repo.attributeList.find { it.name == "path" && it.type == Build.Attribute.Discriminator.STRING }
            ?: return null
    val pathStr = pathAttr.stringValue.ifEmpty { return null }
    val rawPath = java.nio.file.Paths.get(pathStr)
    val repoDir =
        (if (rawPath.isAbsolute) rawPath.toFile() else workingDirectory.resolve(rawPath).toFile())
    if (!repoDir.exists() || !repoDir.isDirectory) return null

    return try {
      val digest = java.security.MessageDigest.getInstance("SHA-256")
      repoDir
          .walkTopDown()
          .filter { it.isFile }
          // Skip MODULE.bazel.lock: bazel auto-regenerates it on every invocation in ways
          // that don't reflect a real source change (it depends on resolution state). Letting
          // it flip the content hash makes generate-hashes non-deterministic across runs.
          .filter { it.name != "MODULE.bazel.lock" }
          .map { Pair(it.relativeTo(repoDir).invariantSeparatorsPath, it) }
          .sortedBy { it.first }
          .forEach { (relPath, file) ->
            digest.update(relPath.toByteArray(Charsets.UTF_8))
            digest.update(0x00)
            digest.update(file.readBytes())
            digest.update(0x00)
          }
      digest.digest().joinToString("") { "%02x".format(it) }
    } catch (e: Exception) {
      logger.w { "Failed to content-hash local repo at $repoDir: ${e.message}" }
      null
    }
  }

  /**
   * Discovers the root module's apparent→canonical repo name mapping by running
   * `bazel mod dump_repo_mapping ""`. Returns a map of apparent name → canonical name.
   * Filters out internal repos (bazel_tools, _builtins, local_config_*) that aren't
   * relevant for dependency hashing.
   */
  @OptIn(ExperimentalCoroutinesApi::class)
  private suspend fun discoverRepoMapping(): Map<String, String> {
    val cmd: MutableList<String> =
        ArrayList<String>().apply {
          add(bazelPath.toString())
          if (noBazelrc) {
            add("--bazelrc=/dev/null")
          }
          addAll(startupOptions)
          add("mod")
          add("dump_repo_mapping")
          // Empty string = root module's repo mapping
          add("")
        }

    logger.i { "Discovering repo mapping: ${cmd.joinToString()}" }
    val result =
        process(
            *cmd.toTypedArray(),
            stdout = Redirect.CAPTURE,
            workingDirectory = workingDirectory.toFile(),
            stderr = Redirect.PRINT,
            destroyForcibly = true,
        )

    if (result.resultCode != 0) {
      logger.w { "bazel mod dump_repo_mapping failed (exit code ${result.resultCode})" }
      return emptyMap()
    }

    return try {
      val mapping = mutableMapOf<String, String>()
      for (line in result.output) {
        val trimmed = line.trim()
        if (trimmed.isEmpty()) continue
        val json = com.google.gson.JsonParser.parseString(trimmed).asJsonObject
        for ((apparent, canonicalElem) in json.entrySet()) {
          val canonical = canonicalElem.asString
          // Skip internal/infrastructure repos not relevant for dependency hashing.
          if (apparent.isEmpty() ||
              canonical.isEmpty() ||
              canonical.startsWith("bazel_tools") ||
              canonical.startsWith("_builtins") ||
              canonical.startsWith("local_config_") ||
              canonical.startsWith("rules_java_builtin") ||
              apparent == "bazel_tools" ||
              apparent == "local_config_platform")
              continue
          mapping[apparent] = canonical
        }
      }
      mapping
    } catch (e: Exception) {
      logger.w { "Failed to parse dump_repo_mapping output: ${e.message}" }
      emptyMap()
    }
  }

  private fun toBazelTarget(target: Build.Target): BazelTarget? {
    return when (target.type) {
      Build.Target.Discriminator.RULE -> BazelTarget.Rule(target)
      Build.Target.Discriminator.SOURCE_FILE -> BazelTarget.SourceFile(target)
      Build.Target.Discriminator.GENERATED_FILE -> BazelTarget.GeneratedFile(target)
      else -> {
        logger.w { "Unsupported target type in the build graph: ${target.type.name}" }
        null
      }
    }
  }
}
