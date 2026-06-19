package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.hasSize
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
import assertk.assertions.isNotNull
import assertk.assertions.isNull
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
  fun parseModuleGraph_withEmptyName_skipsModule() {
    // Reproduces the JSON shape `bazel mod graph --output=json` emits for an
    // unnamed root MODULE.bazel.
    val json =
        """
      {
        "key": "<root>",
        "name": "",
        "version": "",
        "apparentName": "",
        "dependencies": [
          {"key": "platforms@1.0.0", "name": "platforms", "version": "1.0.0", "apparentName": "platforms"}
        ]
      }
    """
            .trimIndent()

    val result = parser.parseModuleGraph(json)

    assertThat(result).hasSize(1)
    assertThat(result["platforms@1.0.0"]).isNotNull()
    assertThat(result["<root>"]).isNull()
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
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
        )
    val newGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "protobuf@21.7" to Module("protobuf@21.7", "protobuf", "21.7", "com_google_protobuf"))

    val result = parser.findChangedModules(oldGraph, newGraph)

    assertThat(result).hasSize(1)
    assertThat(result).containsExactlyInAnyOrder("protobuf@21.7")
  }

  @Test
  fun findChangedModules_withRemovedModule_returnsRemovedModuleKey() {
    val oldGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
            "protobuf@21.7" to Module("protobuf@21.7", "protobuf", "21.7", "com_google_protobuf"))
    val newGraph =
        mapOf(
            "root" to Module("root", "my-project", "1.0.0", "my-project"),
        )

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
            "protobuf@21.7" to Module("protobuf@21.7", "protobuf", "21.7", "com_google_protobuf"))
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

  // ---------------------------------------------------------------------------------------
  // breakCycles
  // ---------------------------------------------------------------------------------------

  @Test
  fun breakCycles_acyclicInput_returnsEdgesUnchanged() {
    val edges = mapOf("a" to listOf("b", "c"), "b" to listOf("c"), "c" to emptyList())

    val result = parser.breakCycles(edges)

    assertThat(result["a"]!!).containsExactlyInAnyOrder("b", "c")
    assertThat(result["b"]!!).containsExactlyInAnyOrder("c")
    assertThat(result["c"]!!).isEmpty()
  }

  @Test
  fun breakCycles_twoNodeCycle_dropsOneEdge() {
    // The real-world case: rules_go <-> gazelle. Adding both rule_inputs
    // makes RuleHasher recurse infinitely; we keep exactly one direction.
    val edges = mapOf("gazelle" to listOf("rules_go"), "rules_go" to listOf("gazelle"))

    val result = parser.breakCycles(edges)

    val total = result.values.sumOf { it.size }
    assertThat(total).isEqualTo(1)
    // Deterministic: sorted DFS starts at "gazelle" first, so its edge survives
    // and rules_go's back-edge is the one that gets dropped.
    assertThat(result["gazelle"]!!).containsExactlyInAnyOrder("rules_go")
    assertThat(result["rules_go"]!!).isEmpty()
  }

  @Test
  fun breakCycles_threeNodeCycle_breaksCycleDeterministically() {
    val edges = mapOf("a" to listOf("b"), "b" to listOf("c"), "c" to listOf("a"))

    val result = parser.breakCycles(edges)

    // Whatever the algorithm picks, the result must be a DAG: total edges = nodes - 1
    // (otherwise the algorithm would have kept a cycle), and both forward edges survive
    // because DFS visits a -> b -> c first and then c -> a is the back-edge.
    assertThat(result["a"]!!).containsExactlyInAnyOrder("b")
    assertThat(result["b"]!!).containsExactlyInAnyOrder("c")
    assertThat(result["c"]!!).isEmpty()
  }

  @Test
  fun breakCycles_selfLoop_dropsSelfEdge() {
    val edges = mapOf("a" to listOf("a", "b"), "b" to emptyList())

    val result = parser.breakCycles(edges)

    assertThat(result["a"]!!).containsExactlyInAnyOrder("b")
  }

  @Test
  fun breakCycles_isDeterministic() {
    val edges = mapOf("gazelle" to listOf("rules_go"), "rules_go" to listOf("gazelle"))

    val first = parser.breakCycles(edges)
    val second = parser.breakCycles(edges)

    assertThat(first).isEqualTo(second)
  }

  // ---------------------------------------------------------------------------------------
  // parseModuleGraphEdges / findTransitiveDependents (issue #197 expansion)
  // ---------------------------------------------------------------------------------------
  // Edge / transitive-dependents tests are regression coverage for issue #197: when
  // `@inner_repo` is the user-listed fine-grained repo and `@middle_repo` wraps it, the
  // expansion has to follow `bazel mod graph` backwards to add `@middle_repo` automatically.

  @Test
  fun parseModuleGraphEdges_realIssue197Shape_extractsEdgesAndRoot() {
    // Mirrors `bazel mod graph --output=json` for the `wrapped_external_repo` fixture:
    // <root> -> inner_repo, middle_repo; middle_repo -> inner_repo (unexpanded reference).
    val json =
        """
      {
        "key": "<root>",
        "name": "wrapped_external_repo_test",
        "version": "0.0.0",
        "apparentName": "wrapped_external_repo_test",
        "dependencies": [
          {
            "key": "inner_repo@_",
            "name": "inner_repo",
            "version": "0.0.0",
            "apparentName": "inner_repo",
            "dependencies": []
          },
          {
            "key": "middle_repo@_",
            "name": "middle_repo",
            "version": "0.0.0",
            "apparentName": "middle_repo",
            "dependencies": [
              {
                "key": "inner_repo@_",
                "name": "inner_repo",
                "version": "0.0.0",
                "apparentName": "inner_repo",
                "unexpanded": true
              }
            ]
          }
        ]
      }
    """
            .trimIndent()

    val graph = parser.parseModuleGraphEdges(json)

    assertThat(graph.rootApparentNames).containsExactlyInAnyOrder("wrapped_external_repo_test")
    assertThat(graph.edges["wrapped_external_repo_test"])
        .isNotNull()
        .containsExactlyInAnyOrder("inner_repo", "middle_repo")
    assertThat(graph.edges["middle_repo"]).isNotNull().containsExactlyInAnyOrder("inner_repo")
    // inner_repo has no outgoing deps.
    assertThat(graph.edges["inner_repo"]).isNotNull().isEmpty()
  }

  @Test
  fun findTransitiveDependents_issue197_addsMiddleRepoButNotRoot() {
    // Same shape as parseModuleGraphEdges_realIssue197Shape: <root> wraps both, middle_repo wraps
    // inner_repo. Listing `inner_repo` as a target must surface `middle_repo` (so wrapper-only
    // consumers in the main repo get correctly invalidated) and must NOT surface the root
    // (the root is the main repo, already queried via `//...:all-targets`).
    val edges =
        mapOf(
            "wrapped_external_repo_test" to setOf("inner_repo", "middle_repo"),
            "middle_repo" to setOf("inner_repo"),
            "inner_repo" to emptySet(),
        )

    val result =
        parser.findTransitiveDependents(
            edges,
            targets = setOf("inner_repo"),
            rootApparentNames = setOf("wrapped_external_repo_test"))

    assertThat(result).containsExactlyInAnyOrder("middle_repo")
  }

  @Test
  fun findTransitiveDependents_multiLevelWrapping_followsChain() {
    // inner -> middle -> outer -> root. Listing inner must add both middle and outer.
    val edges =
        mapOf(
            "root_app" to setOf("outer"),
            "outer" to setOf("middle"),
            "middle" to setOf("inner"),
            "inner" to emptySet(),
        )

    val result =
        parser.findTransitiveDependents(
            edges, targets = setOf("inner"), rootApparentNames = setOf("root_app"))

    assertThat(result).containsExactlyInAnyOrder("middle", "outer")
  }

  @Test
  fun findTransitiveDependents_unrelatedRepos_returnsEmpty() {
    // A repo with no chain to the targets should not be expanded into.
    val edges =
        mapOf(
            "root_app" to setOf("inner", "unrelated"),
            "inner" to emptySet(),
            "unrelated" to emptySet(),
        )

    val result =
        parser.findTransitiveDependents(
            edges, targets = setOf("inner"), rootApparentNames = setOf("root_app"))

    assertThat(result).isEmpty()
  }

  @Test
  fun findTransitiveDependents_emptyTargets_returnsEmpty() {
    val edges = mapOf("a" to setOf("b"), "b" to emptySet())

    val result =
        parser.findTransitiveDependents(edges, targets = emptySet(), rootApparentNames = emptySet())

    assertThat(result).isEmpty()
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
