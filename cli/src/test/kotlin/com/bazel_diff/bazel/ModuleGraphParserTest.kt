package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.hasSize
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
import assertk.assertions.isNotNull
import org.junit.Test

class ModuleGraphParserTest {
  private val parser = ModuleGraphParser()

  @Test
  fun parseModuleGraph_withValidJson_extractsModules() {
    val json =
        """
      {
        "key": "root",
        "name": "my-project",
        "version": "1.0.0",
        "apparentName": "my-project",
        "dependencies": []
      }
    """
            .trimIndent()

    val result = parser.parseModuleGraph(json)

    assertThat(result).hasSize(1)
    val module = result["root"]
    assertThat(module).isNotNull()
    assertThat(module!!.key).isEqualTo("root")
    assertThat(module.name).isEqualTo("my-project")
    assertThat(module.version).isEqualTo("1.0.0")
    assertThat(module.apparentName).isEqualTo("my-project")
  }

  @Test
  fun parseModuleGraph_withNestedDependencies_extractsAllModules() {
    val json =
        """
      {
        "key": "root",
        "name": "my-project",
        "version": "1.0.0",
        "apparentName": "my-project",
        "dependencies": [
          {
            "key": "abseil-cpp@20240116.2",
            "name": "abseil-cpp",
            "version": "20240116.2",
            "apparentName": "com_google_absl",
            "dependencies": [
              {
                "key": "googletest@1.14.0",
                "name": "googletest",
                "version": "1.14.0",
                "apparentName": "com_google_googletest",
                "dependencies": []
              }
            ]
          },
          {
            "key": "protobuf@21.7",
            "name": "protobuf",
            "version": "21.7",
            "apparentName": "com_google_protobuf",
            "dependencies": []
          }
        ]
      }
    """
            .trimIndent()

    val result = parser.parseModuleGraph(json)

    assertThat(result).hasSize(4)
    assertThat(result.keys)
        .containsExactlyInAnyOrder(
            "root", "abseil-cpp@20240116.2", "googletest@1.14.0", "protobuf@21.7")

    val abseil = result["abseil-cpp@20240116.2"]
    assertThat(abseil).isNotNull()
    assertThat(abseil!!.name).isEqualTo("abseil-cpp")
    assertThat(abseil.version).isEqualTo("20240116.2")

    val googletest = result["googletest@1.14.0"]
    assertThat(googletest).isNotNull()
    assertThat(googletest!!.version).isEqualTo("1.14.0")
  }

  @Test
  fun parseModuleGraph_withStderrPrefix_extractsModules() {
    val cleanJson =
        """
      {
        "key": "<root>",
        "name": "ws",
        "version": "",
        "apparentName": "ws",
        "dependencies": [
          {"key": "a@1", "name": "a", "version": "1", "apparentName": "a"}
        ]
      }
    """
            .trimIndent()
    val polluted = "INFO: Invocation ID: abc\nLoading: 0 packages loaded\n$cleanJson"

    val clean = parser.parseModuleGraph(cleanJson)
    val actual = parser.parseModuleGraph(polluted)

    assertThat(actual).hasSize(2)
    assertThat(actual).isEqualTo(clean)
  }

  @Test
  fun parseModuleGraph_withInvalidJson_returnsEmptyMap() {
    val json = "{ invalid json"

    val result = parser.parseModuleGraph(json)

    assertThat(result).isEmpty()
  }

  @Test
  fun parseModuleGraph_withEmptyJson_returnsEmptyMap() {
    val json = "{}"

    val result = parser.parseModuleGraph(json)

    assertThat(result).isEmpty()
  }

  @Test
  fun parseModuleGraph_withIncompleteModule_skipsModule() {
    val json =
        """
      {
        "key": "root",
        "name": "my-project",
        "version": "1.0.0",
        "dependencies": [
          {
            "key": "incomplete",
            "name": "incomplete-module"
          }
        ]
      }
    """
            .trimIndent()

    val result = parser.parseModuleGraph(json)

    // Root module is incomplete (missing apparentName), so nothing should be extracted
    assertThat(result).isEmpty()
  }

  @Test
  fun findChangedModules_withNoChanges_returnsEmptySet() {
    val oldGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240116.2", "com_google_absl"))
    val newGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240116.2", "com_google_absl"))

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).isEmpty()
  }

  @Test
  fun findChangedModules_withAddedModule_returnsAddedModuleKey() {
    val oldGraph =
        mapOf("root" to Module("root", "my-project", "1.0.0", "my-project"),)
    val newGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "protobuf@21.7" to
                Module("protobuf@21.7", "protobuf", "21.7", "com_google_protobuf"))

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).hasSize(1)
    assertThat(result).containsExactlyInAnyOrder("protobuf@21.7")
  }

  @Test
  fun findChangedModules_withRemovedModule_returnsRemovedModuleKey() {
    val oldGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "protobuf@21.7" to
                Module("protobuf@21.7", "protobuf", "21.7", "com_google_protobuf"))
    val newGraph =
        mapOf("root" to Module("root", "my-project", "1.0.0", "my-project"),)

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).hasSize(1)
    assertThat(result).containsExactlyInAnyOrder("protobuf@21.7")
  }

  @Test
  fun findChangedModules_withVersionChange_returnsChangedModuleKey() {
    val oldGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240116.2", "com_google_absl"))
    val newGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240722.0", "com_google_absl"))

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).hasSize(1)
    assertThat(result).containsExactlyInAnyOrder("abseil-cpp@20240116.2")
  }

  @Test
  fun findChangedModules_withMultipleChanges_returnsAllChangedKeys() {
    val oldGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240116.2", "com_google_absl"),
            "protobuf@21.7" to
                Module("protobuf@21.7", "protobuf", "21.7", "com_google_protobuf"))
    val newGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240722.0", "com_google_absl"),
            "googletest@1.14.0" to
                Module("googletest@1.14.0", "googletest", "1.14.0", "com_google_googletest"))

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).hasSize(3)
    assertThat(result)
        .containsExactlyInAnyOrder("abseil-cpp@20240116.2", "protobuf@21.7", "googletest@1.14.0")
  }

  @Test
  fun findChangedModules_withEmptyGraphs_returnsEmptySet() {
    val oldGraph = emptyMap<String, Module>()
    val newGraph = emptyMap<String, Module>()

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).isEmpty()
  }

  @Test
  fun findChangedModules_withOldGraphEmpty_returnsAllNewModuleKeys() {
    val oldGraph = emptyMap<String, Module>()
    val newGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240116.2", "com_google_absl"))

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).hasSize(2)
    assertThat(result).containsExactlyInAnyOrder("root", "abseil-cpp@20240116.2")
  }

  @Test
  fun findChangedModules_withNewGraphEmpty_returnsAllOldModuleKeys() {
    val oldGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "abseil-cpp@20240116.2" to
                Module("abseil-cpp@20240116.2", "abseil-cpp", "20240116.2", "com_google_absl"))
    val newGraph = emptyMap<String, Module>()

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).hasSize(2)
    assertThat(result).containsExactlyInAnyOrder("root", "abseil-cpp@20240116.2")
  }
}
