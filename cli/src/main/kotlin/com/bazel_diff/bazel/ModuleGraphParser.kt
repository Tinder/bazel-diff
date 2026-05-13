package com.bazel_diff.bazel

import com.google.gson.JsonObject
import com.google.gson.JsonParser

/**
 * Data class representing a module in the dependency graph.
 */
data class Module(
    val key: String,
    val name: String,
    val version: String,
    val apparentName: String
)

/**
 * Parses and compares Bazel module graphs to detect changes.
 *
 * Instead of including the entire module graph in the hash seed (which causes all targets
 * to rehash when MODULE.bazel changes), this class identifies which specific modules changed
 * so we can query only the targets that depend on those modules.
 */
class ModuleGraphParser {
  /**
   * Parses the JSON output from `bazel mod graph --output=json`.
   *
   * Tolerates a non-JSON prefix (e.g. leaked stderr from bazel-diff
   * 17.0.1..18.0.5, which captured stderr into moduleGraphJson via
   * Process.kt's captureAll -> ProcessBuilder.redirectErrorStream(true)).
   *
   * @param json The JSON string from bazel mod graph
   * @return A map of module keys to Module objects
   */
  fun parseModuleGraph(json: String): Map<String, Module> {
    val modules = mutableMapOf<String, Module>()

    try {
      val root = try {
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

    if (key != null && name != null && version != null && apparentName != null) {
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
   * Module names (the `name` field of the `module(name = ...)` declaration) are used as the
   * key here because the alternative -- `module_key` -- is not always populated on the
   * `Build.Repository` protos returned by `bazel mod show_repo`, which is what consumers want
   * to look up against. Module names are universally present and sufficient to find a unique
   * row in the graph for the common no-multi-version case.
   *
   * The same module may appear in multiple places in the JSON tree (`bazel mod graph` inlines
   * each module once and references it via `unexpanded` afterwards). This method walks every
   * `dependencies` array it sees, so even the `unexpanded` references contribute an edge. The
   * resulting map is keyed by the parent's `module_name` and contains the union of all direct
   * dep names observed across the tree.
   *
   * Returns an empty map on parse failure (same tolerance as [parseModuleGraph]).
   */
  fun parseModuleGraphDepEdges(json: String): Map<String, List<String>> {
    val edges = mutableMapOf<String, MutableSet<String>>()
    try {
      val root = try {
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
