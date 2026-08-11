package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isNull
import assertk.assertions.isTrue
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import java.io.File
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.get

class BazelModServiceTest : KoinTest {

  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private fun fakeBazel(
      name: String = "bazel",
      body: String,
  ): File =
      File(temp.root, name).apply {
        writeText("#!/bin/sh\n$body\n")
        setExecutable(true)
      }

  private fun withService(
      workspaceDir: File,
      bazel: File,
      startupOptions: List<String> = listOf("--host_jvm_args=-Xmx1g"),
      noBazelrc: Boolean = true,
      block: suspend (BazelModService) -> Unit,
  ) {
    startKoin {
      modules(
          module {
            single<Logger> { SilentLogger }
            single {
              BazelModService(
                  workingDirectory = workspaceDir.toPath(),
                  bazelPath = bazel.toPath(),
                  startupOptions = startupOptions,
                  noBazelrc = noBazelrc,
              )
            }
          })
    }
    try {
      runBlocking { block(get()) }
    } finally {
      stopKoin()
    }
  }

  private fun workspaceWithModule(): File {
    val workspaceDir = temp.newFolder()
    File(workspaceDir, "MODULE.bazel").writeText("module(name = \"test\")\n")
    return workspaceDir
  }

  @Test
  fun isBzlmodEnabled_returnsFalse_whenWorkspaceHasNoModuleBazel() {
    val workspaceDir = temp.newFolder()
    // Fake bazel that would succeed — without MODULE.bazel the production path still runs
    // `bazel mod graph`; use a failing binary so this stays environment-independent.
    val bazel = fakeBazel(body = "exit 1")
    withService(workspaceDir, bazel) { service -> assertThat(service.isBzlmodEnabled).isFalse() }
  }

  @Test
  fun isBzlmodEnabled_returnsTrue_whenModuleBazelPresentAndBazelSucceeds() {
    val workspaceDir = workspaceWithModule()
    val bazel = fakeBazel(body = "echo 'root'\nexit 0")
    withService(workspaceDir, bazel) { service -> assertThat(service.isBzlmodEnabled).isTrue() }
  }

  @Test
  fun isBzlmodEnabled_returnsFalse_whenBazelExitsNonZero_evenWithModuleBazel() {
    val workspaceDir = workspaceWithModule()
    val bazel = fakeBazel(body = "echo 'ERROR: bzlmod disabled' >&2\nexit 2")
    withService(workspaceDir, bazel) { service -> assertThat(service.isBzlmodEnabled).isFalse() }
  }

  @Test
  fun getModuleGraph_returnsTrimmedOutput_onSuccess() {
    val workspaceDir = workspaceWithModule()
    val bazel =
        fakeBazel(
            body =
                """
                echo '  root'
                echo '  |-- foo@1.0'
                echo ''
                exit 0
                """
                    .trimIndent())
    withService(workspaceDir, bazel) { service ->
      val graph = service.getModuleGraph()
      assertThat(graph).isEqualTo("root\n  |-- foo@1.0")
    }
  }

  @Test
  fun getModuleGraph_returnsNull_whenBzlmodDisabled() {
    val workspaceDir = workspaceWithModule()
    val bazel = fakeBazel(body = "exit 1")
    withService(workspaceDir, bazel) { service -> assertThat(service.getModuleGraph()).isNull() }
  }

  @Test
  fun getModuleGraph_returnsNull_onFailureAfterEnabled() {
    val workspaceDir = workspaceWithModule()
    val countFile = File(temp.root, "mod-graph-count")
    // Escape $ so Kotlin does not treat shell $n as string templates.
    val bazel =
        fakeBazel(
            body =
                """
                n=`cat '${countFile.absolutePath}' 2>/dev/null || echo 0`
                n=`expr "${'$'}n" + 1`
                echo "${'$'}n" > '${countFile.absolutePath}'
                if [ "${'$'}n" -eq 1 ]; then
                  echo 'root'
                  exit 0
                fi
                exit 1
                """
                    .trimIndent())
    withService(workspaceDir, bazel) { service ->
      assertThat(service.isBzlmodEnabled).isTrue()
      assertThat(service.getModuleGraph()).isNull()
    }
  }

  @Test
  fun getModuleGraphJson_returnsTrimmedOutput_onSuccess() {
    val workspaceDir = workspaceWithModule()
    val bazel =
        fakeBazel(
            body =
                """
                case " ${'$'}* " in
                  *" --output=json "*)
                    printf '  {"key":"<root>"}  \n'
                    ;;
                  *)
                    echo 'root'
                    ;;
                esac
                exit 0
                """
                    .trimIndent())
    withService(workspaceDir, bazel) { service ->
      val json = service.getModuleGraphJson()
      assertThat(json).isEqualTo("{\"key\":\"<root>\"}")
    }
  }

  @Test
  fun getModuleGraphJson_returnsNull_whenBzlmodDisabled() {
    val workspaceDir = workspaceWithModule()
    val bazel = fakeBazel(body = "exit 1")
    withService(workspaceDir, bazel) { service ->
      assertThat(service.getModuleGraphJson()).isNull()
    }
  }

  @Test
  fun getModuleGraphJson_returnsNull_onFailureAfterEnabled() {
    val workspaceDir = workspaceWithModule()
    val bazel =
        fakeBazel(
            body =
                """
                case " ${'$'}* " in
                  *" --output=json "*)
                    exit 1
                    ;;
                  *)
                    echo 'root'
                    exit 0
                    ;;
                esac
                """
                    .trimIndent())
    withService(workspaceDir, bazel) { service ->
      assertThat(service.isBzlmodEnabled).isTrue()
      assertThat(service.getModuleGraphJson()).isNull()
    }
  }

  @Test
  fun checkBzlmodEnabled_addsNoBazelrcAndStartupOptions() {
    val workspaceDir = workspaceWithModule()
    val argsFile = File(temp.root, "bazel-args.txt")
    val bazel =
        fakeBazel(
            body =
                """
                echo "${'$'}@" > '${argsFile.absolutePath}'
                echo 'root'
                exit 0
                """
                    .trimIndent())
    withService(
        workspaceDir,
        bazel,
        startupOptions = listOf("--host_jvm_args=-Xmx512m", "--batch"),
        noBazelrc = true,
    ) { service ->
      assertThat(service.isBzlmodEnabled).isTrue()
      val args = argsFile.readText()
      assertThat(args).contains("--bazelrc=/dev/null")
      assertThat(args).contains("--host_jvm_args=-Xmx512m")
      assertThat(args).contains("--batch")
      assertThat(args).contains("mod")
      assertThat(args).contains("graph")
    }
  }

  @Test
  fun getModuleGraph_omitsNoBazelrc_whenDisabled() {
    val workspaceDir = workspaceWithModule()
    val argsFile = File(temp.root, "bazel-args-nobazelrc-off.txt")
    val bazel =
        fakeBazel(
            body =
                """
                echo "${'$'}@" > '${argsFile.absolutePath}'
                echo 'root'
                exit 0
                """
                    .trimIndent())
    withService(workspaceDir, bazel, noBazelrc = false) { service ->
      service.getModuleGraph()
      val args = argsFile.readText()
      assertThat(args.contains("--bazelrc=/dev/null")).isEqualTo(false)
    }
  }
}
