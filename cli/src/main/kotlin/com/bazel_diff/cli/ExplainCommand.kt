package com.bazel_diff.cli

import com.bazel_diff.cli.converter.ExplainFormatConverter
import com.bazel_diff.di.loggingModule
import com.bazel_diff.di.serialisationModule
import com.bazel_diff.interactor.DeserialiseHashesInteractor
import com.bazel_diff.interactor.ExplainFormat
import com.bazel_diff.interactor.ExplainImpactInteractor
import com.bazel_diff.interactor.ImpactGraphRenderer
import com.bazel_diff.interactor.UnknownTargetException
import com.google.gson.Gson
import java.io.BufferedWriter
import java.io.File
import java.io.FileDescriptor
import java.io.FileWriter
import java.io.IOException
import java.util.concurrent.Callable
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import org.koin.java.KoinJavaComponent
import picocli.CommandLine

/**
 * Explains *why* a single target was impacted: which upstream target(s) actually changed, and along
 * which dependency path the change reached it.
 *
 * Answers [issue #479](https://github.com/Tinder/bazel-diff/issues/479). Pure post-processing over
 * the same three files `get-impacted-targets` consumes -- it runs no Bazel query and needs no
 * workspace, so it is cheap enough to run on a CI failure after the fact.
 */
@CommandLine.Command(
    name = "explain",
    mixinStandardHelpOptions = true,
    description =
        [
            "Explains why a target was impacted: the upstream target(s) whose own hash changed, " +
                "and the dependency path from each down to the queried target. Renders as text, " +
                "JSON, Graphviz DOT, or a Mermaid flowchart."],
    versionProvider = VersionProvider::class)
class ExplainCommand : Callable<Int> {
  @CommandLine.ParentCommand private lateinit var parent: BazelDiff

  @CommandLine.Option(
      names = ["-sh", "--startingHashes"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "The path to the JSON file of target hashes for the initial revision. Run 'generate-hashes' to get this value."],
      required = true)
  lateinit var startingHashesJSONPath: File

  @CommandLine.Option(
      names = ["-fh", "--finalHashes"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "The path to the JSON file of target hashes for the final revision. Run 'generate-hashes' to get this value."],
      required = true)
  lateinit var finalHashesJSONPath: File

  @CommandLine.Option(
      names = ["-d", "--depEdgesFile"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "Path to the dependency-edges file written by 'generate-hashes --depEdgesFile'. " +
                  "Required: attribution is a walk over these edges."],
      required = true)
  lateinit var depEdgesJSONPath: File

  @CommandLine.Option(
      names = ["-t", "--target"],
      scope = CommandLine.ScopeType.LOCAL,
      description = ["The impacted Bazel label to explain, e.g. '//service/a:app'."],
      required = true)
  lateinit var target: String

  @CommandLine.Option(
      names = ["-f", "--format"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "Output format: \${COMPLETION-CANDIDATES}. 'dot' and 'mermaid' emit a node-link " +
                  "graph of the blame subgraph, with edges pointing from root cause to queried " +
                  "target."],
      converter = [ExplainFormatConverter::class],
      defaultValue = "TEXT")
  var format: ExplainFormat = ExplainFormat.TEXT

  @CommandLine.Option(
      names = ["-o", "--output"],
      scope = CommandLine.ScopeType.LOCAL,
      description = ["Filepath to write the explanation to. Defaults to STDOUT."])
  var outputPath: File? = null

  @CommandLine.Option(
      names = ["--maxRootCauses"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "Report at most this many root causes, nearest first. The full count is always " +
                  "reported alongside. 0 means no limit. Default: \${DEFAULT-VALUE}."],
      defaultValue = "25")
  var maxRootCauses: Int = 25

  @CommandLine.Option(
      names = ["--maxDepth"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "Stop searching this many dependency hops above the queried target. Root causes " +
                  "further upstream are then not reported (a warning is logged). -1 means no " +
                  "bound. Default: \${DEFAULT-VALUE}."],
      defaultValue = "-1")
  var maxDepth: Int = -1

  @CommandLine.Spec lateinit var spec: CommandLine.Model.CommandSpec

  override fun call(): Int {
    org.koin.core.context.GlobalContext.stopKoin()
    startKoin { modules(serialisationModule(), loggingModule(parent.isVerbose())) }

    return try {
      validate()
      val deserialiser = DeserialiseHashesInteractor()
      val from = deserialiser.executeTargetHash(startingHashesJSONPath)
      val to = deserialiser.executeTargetHash(finalHashesJSONPath)
      val depEdges = deserialiser.deserializeDeps(depEdgesJSONPath)

      val explanation =
          ExplainImpactInteractor()
              .explain(from, to, depEdges, target, maxDepth = maxDepth, maxRoots = maxRootCauses)

      val gson = KoinJavaComponent.get<Gson>(Gson::class.java)
      val rendered = ImpactGraphRenderer(gson).render(explanation, format)

      try {
        BufferedWriter(
                when (val path = outputPath) {
                  null -> FileWriter(FileDescriptor.out)
                  else -> FileWriter(path)
                })
            .use { it.write(rendered) }
        CommandLine.ExitCode.OK
      } catch (e: IOException) {
        CommandLine.ExitCode.SOFTWARE
      }
    } catch (e: UnknownTargetException) {
      throw CommandLine.ParameterException(spec.commandLine(), e.message)
    } finally {
      stopKoin()
    }
  }

  private fun validate() {
    if (!startingHashesJSONPath.canRead()) {
      throw CommandLine.ParameterException(
          spec.commandLine(), "Incorrect starting hashes: file doesn't exist or can't be read.")
    }
    if (!finalHashesJSONPath.canRead()) {
      throw CommandLine.ParameterException(
          spec.commandLine(), "Incorrect final hashes: file doesn't exist or can't be read.")
    }
    if (!depEdgesJSONPath.canRead()) {
      throw CommandLine.ParameterException(
          spec.commandLine(), "Incorrect dep edges file: file doesn't exist or can't be read.")
    }
  }
}
