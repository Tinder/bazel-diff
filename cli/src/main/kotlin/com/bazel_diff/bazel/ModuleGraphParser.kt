package com.bazel_diff.bazel

import com.google.gson.JsonObject
import com.google.gson.JsonParser

/** Data class representing a module in the dependency graph. */
data class Module(val key: String, val name: String, val version: String, val apparentName: String)

/**
 * Parses and compares Bazel module graphs to detect changes.
 *
 * Instead of including the entire module graph in the hash seed (which causes all targets to rehash
 * when MODULE.bazel changes), this class identifies which specific modules changed so we can query
 * only the targets that depend on those modules.
 */
class ModuleGraphParser {
  /**
   * Parses the JSON output from `bazel mod graph --output=json`.
   *
   * Tolerates a non-JSON prefix (e.g. leaked stderr from bazel-diff 17.0.1..18.0.5, which captured
   * stderr into moduleGraphJson via Process.kt's captureAll ->
   * ProcessBuilder.redirectErrorStream(true)).
   *
   * @param json The JSON string from bazel mod graph
   * @return A map of module keys to Module objects
   */
  fun parseModuleGraph(json: String): Map<String, Module> {
    val modules = mutableMapOf<String, Module>()

    try {
      val root =
          try {
            JsonParser.parseString(json).asJsonObject
          } catch (_: Exception) {
            val start = json.indexOf('{')
            if (start < 0) return emptyMap()
            JsonParser.parseString(json.substring(start)).asJsonObject
          }
      extractModules(root, modules)
    } catch (e: Exception) {
      // If parsing fails, return empty map
      return emptyMap()
    }

    return modules
  }

  private fun extractModules(obj: JsonObject, modules: MutableMap<String, Module>) {
    val key = obj.get("key")?.asString
    val name = obj.get("name")?.asString
    val version = obj.get("version")?.asString
    val apparentName = obj.get("apparentName")?.asString

    // Bazel's MODULE.bazel spec requires module names to be non-empty; reject
    // empty `name` so the synthetic `<root>` entry of an unnamed MODULE.bazel
    // doesn't reach downstream canonical-repo matching.
    if (key != null && !name.isNullOrEmpty() && version != null && apparentName != null) {
      modules[key] = Module(key, name, version, apparentName)
    }

    // Recursively extract from dependencies
    obj.get("dependencies")?.asJsonArray?.forEach { dep ->
      if (dep.isJsonObject) {
        extractModules(dep.asJsonObject, modules)
      }
    }
  }

  /**
   * Parses the JSON from `bazel mod graph --output=json` and returns each module's direct
   * `bazel_dep` neighbours as a `module_name -> [dep_module_name, ...]` map.
   *
   * Module names (the `name` field of the `module(name = ...)` declaration) are used as the key
   * here because the alternative -- `module_key` -- is not always populated on the
   * `Build.Repository` protos returned by `bazel mod show_repo`, which is what consumers want to
   * look up against. Module names are universally present and sufficient to find a unique row in
   * the graph for the common no-multi-version case.
   *
   * The same module may appear in multiple places in the JSON tree (`bazel mod graph` inlines each
   * module once and references it via `unexpanded` afterwards). This method walks every
   * `dependencies` array it sees, so even the `unexpanded` references contribute an edge. The
   * resulting map is keyed by the parent's `module_name` and contains the union of all direct dep
   * names observed across the tree.
   *
   * Returns an empty map on parse failure (same tolerance as [parseModuleGraph]).
   */
  fun parseModuleGraphDepEdges(json: String): Map<String, List<String>> {
    val edges = mutableMapOf<String, MutableSet<String>>()
    try {
      val root =
          try {
            JsonParser.parseString(json).asJsonObject
          } catch (_: Exception) {
            val start = json.indexOf('{')
            if (start < 0) return emptyMap()
            JsonParser.parseString(json.substring(start)).asJsonObject
          }
      extractDepEdges(root, edges)
    } catch (_: Exception) {
      return emptyMap()
    }
    return edges.mapValues { it.value.toList() }
  }

  private fun extractDepEdges(obj: JsonObject, edges: MutableMap<String, MutableSet<String>>) {
    val name = obj.get("name")?.asString ?: return
    val deps = obj.get("dependencies")?.asJsonArray ?: return
    val collected = edges.getOrPut(name) { mutableSetOf() }
    for (dep in deps) {
      if (!dep.isJsonObject) continue
      val depObj = dep.asJsonObject
      val depName = depObj.get("name")?.asString ?: continue
      collected.add(depName)
      // Even if this child is `unexpanded`, recurse to pick up edges from its own expansion
      // elsewhere in the tree.
      extractDepEdges(depObj, edges)
    }
  }

  /**
   * Returns a copy of [edges] with back-edges removed so the result is acyclic.
   *
   * `bazel mod graph` legitimately contains cycles: for example `rules_go` declares `bazel_dep(name
   * = "gazelle", dev_dependency = True)` while `gazelle` declares `bazel_dep(name = "rules_go")`,
   * so the dep graph has `rules_go <-> gazelle`. Feeding both edges into
   * [BazelQueryService.queryBzlmodRepos] as `rule_input`s on the synthetic `//external:*` targets
   * makes `RuleHasher` recurse infinitely and throw `CircularDependencyException`. We need a
   * cycle-free dep DAG before emitting edges.
   *
   * The algorithm is a single DFS, visiting nodes in lexicographic order with their out-edges also
   * sorted. An edge to a node currently on the DFS path is a back-edge (it would close a cycle) and
   * is dropped; every other edge is kept. The result is therefore (a) acyclic and (b) deterministic
   * across runs.
   *
   * Dropping the back-edge is conservative: a content change in the dropped-edge target still
   * surfaces via its own synthetic `//external:*` target's hash (each repo gets one), so main-repo
   * consumers that depend on either side of the cycle still see the change. We only lose the
   * ability to propagate through the cycle itself, which is fine because all SCC members are
   * co-dependent and a change in any of them already invalidates their own hashes directly.
   */
  fun breakCycles(edges: Map<String, List<String>>): Map<String, List<String>> {
    val result = mutableMapOf<String, List<String>>()
    val visited = mutableSetOf<String>()
    val onPath = mutableSetOf<String>()

    fun dfs(node: String) {
      if (node in visited) return
      onPath.add(node)
      val kept = mutableListOf<String>()
      for (target in edges[node].orEmpty().sorted()) {
        if (target in onPath) continue // back-edge
        kept.add(target)
        dfs(target)
      }
      result[node] = kept
      onPath.remove(node)
      visited.add(node)
    }

    for (node in edges.keys.sorted()) dfs(node)
    return result
  }

  /**
   * Result of parsing the bzlmod dependency graph as edges keyed by `apparentName`.
   *
   * Distinct from [parseModuleGraphDepEdges] (which is keyed by `name` for use by
   * [BazelQueryService.queryBzlmodRepos] when synthesising `//external:<name>` rule_input edges):
   * this view keys by `apparentName` because the consumer is `--fineGrainedHashExternalRepos`,
   * whose tokens are apparent repo names (e.g. `@inner_repo`).
   *
   * @property edges Map from a module's `apparentName` to the set of `apparentName`s of its direct
   *   bzlmod dependencies. Populated for every module reached when walking the JSON tree.
   * @property rootApparentNames `apparentName`s for the root module(s) (key `<root>`). The root is
   *   the main repo and is always queried via `//...:all-targets`, so dependents-expansion should
   *   never add it to a fine-grained set.
   */
  data class GraphEdges(
      val edges: Map<String, Set<String>>,
      val rootApparentNames: Set<String>,
  )

  /**
   * Parses `bazel mod graph --output=json` into apparent-name dependency edges. Tolerates a
   * non-JSON prefix (e.g. leaked stderr) using the same recovery as [parseModuleGraph].
   *
   * Each module in the JSON tree contributes an edge for every entry in its `dependencies` array.
   * `unexpanded` dependency stubs (modules that the bzlmod resolver already described elsewhere in
   * the graph) still contribute the edge but are not recursed into to avoid duplicate work.
   */
  fun parseModuleGraphEdges(json: String): GraphEdges {
    val edges = mutableMapOf<String, MutableSet<String>>()
    val rootApparentNames = mutableSetOf<String>()
    try {
      val root =
          try {
            JsonParser.parseString(json).asJsonObject
          } catch (_: Exception) {
            val start = json.indexOf('{')
            if (start < 0) return GraphEdges(emptyMap(), emptySet())
            JsonParser.parseString(json.substring(start)).asJsonObject
          }
      walkEdges(root, edges, rootApparentNames, isRoot = true)
    } catch (_: Exception) {
      return GraphEdges(emptyMap(), emptySet())
    }
    return GraphEdges(edges.mapValues { it.value.toSet() }, rootApparentNames.toSet())
  }

  private fun walkEdges(
      obj: JsonObject,
      edges: MutableMap<String, MutableSet<String>>,
      rootApparentNames: MutableSet<String>,
      isRoot: Boolean,
  ) {
    val apparentName = obj.get("apparentName")?.asString ?: return
    if (isRoot) rootApparentNames.add(apparentName)
    val mySet = edges.getOrPut(apparentName) { mutableSetOf() }
    val deps = obj.get("dependencies")?.asJsonArray ?: return
    for (dep in deps) {
      if (!dep.isJsonObject) continue
      val depObj = dep.asJsonObject
      val depApparent = depObj.get("apparentName")?.asString ?: continue
      mySet.add(depApparent)
      val unexpanded = depObj.get("unexpanded")?.asBoolean ?: false
      if (!unexpanded) {
        walkEdges(depObj, edges, rootApparentNames, isRoot = false)
      }
    }
  }

  /**
   * Computes the set of `apparentName`s that transitively depend on any module in [targets] by
   * traversing [edges] in reverse. Excludes [rootApparentNames] from the result.
   *
   * Background: `--fineGrainedHashExternalRepos` opts a leaf module (e.g. `@inner_repo`) into
   * per-target hashing. When another bzlmod module (`@middle_repo`) wraps it via alias or re-export
   * and the main repo depends only on the wrapper, the wrapper's targets must also be queried for
   * the hash chain to reach the leaf. This is the engine for that auto-expansion (issue #197). The
   * set returned here is what the caller should add to the user-supplied set.
   */
  fun findTransitiveDependents(
      edges: Map<String, Set<String>>,
      targets: Set<String>,
      rootApparentNames: Set<String>,
  ): Set<String> {
    if (edges.isEmpty() || targets.isEmpty()) return emptySet()
    val reverse = mutableMapOf<String, MutableSet<String>>()
    for ((from, deps) in edges) {
      for (dep in deps) {
        reverse.getOrPut(dep) { mutableSetOf() }.add(from)
      }
    }
    val out = mutableSetOf<String>()
    val visited = targets.toMutableSet()
    val queue: ArrayDeque<String> = ArrayDeque(targets)
    while (queue.isNotEmpty()) {
      val cur = queue.removeFirst()
      val parents = reverse[cur] ?: continue
      for (p in parents) {
        if (p in visited) continue
        visited.add(p)
        if (p !in rootApparentNames) out.add(p)
        queue.addLast(p)
      }
    }
    return out
  }

  /**
   * Compares two module graphs and returns the keys of modules that changed.
   *
   * A module is considered changed if:
   * - It exists in the new graph but not the old graph (added)
   * - It exists in the old graph but not the new graph (removed)
   * - It exists in both but has a different version
   *
   * @param oldGraph Module graph from the starting revision
   * @param newGraph Module graph from the final revision
   * @return Set of module keys that changed
   */
  fun findChangedModules(
      oldGraph: Map<String, Module>,
      newGraph: Map<String, Module>
  ): Set<String> {
    val changed = mutableSetOf<String>()

    // Find added and version-changed modules
    newGraph.forEach { (key, newModule) ->
      val oldModule = oldGraph[key]
      if (oldModule == null) {
        // Module was added
        changed.add(key)
      } else if (oldModule.version != newModule.version) {
        // Module version changed
        changed.add(key)
      }
    }

    // Find removed modules
    oldGraph.keys.forEach { key ->
      if (!newGraph.containsKey(key)) {
        changed.add(key)
      }
    }

    return changed
  }
}
