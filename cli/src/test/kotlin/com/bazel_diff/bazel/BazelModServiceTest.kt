package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.isFalse
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import java.io.File
import java.nio.file.Paths
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

  @Test
  fun isBzlmodEnabled_returnsFalse_whenWorkspaceHasNoModuleBazel() {
    val workspaceDir = temp.newFolder()
    startKoin {
      modules(
          module {
            single<Logger> { SilentLogger }
            single {
              BazelModService(
                  workingDirectory = workspaceDir.toPath(),
                  bazelPath = Paths.get("bazel"),
                  startupOptions = listOf("--enable_bzlmod"),
                  noBazelrc = true,
              )
            }
          })
    }
    try {
      val service = get<BazelModService>()
      assertThat(service.isBzlmodEnabled).isFalse()
    } finally {
      stopKoin()
    }
  }

  @Test
  fun isBzlmodEnabled_returnsConsistentValue_whenWorkspaceHasModuleBazel() {
    val workspaceDir = temp.newFolder()
    File(workspaceDir, "MODULE.bazel").writeText("module(name = \"test\")\n")
    startKoin {
      modules(
          module {
            single<Logger> { SilentLogger }
            single {
              BazelModService(
                  workingDirectory = workspaceDir.toPath(),
                  bazelPath = Paths.get("bazel"),
                  startupOptions = listOf("--enable_bzlmod"),
                  noBazelrc = true,
              )
            }
          })
    }
    try {
      val service = get<BazelModService>()
      service.isBzlmodEnabled
    } finally {
      stopKoin()
    }
  }
}
