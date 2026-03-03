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
   * @param json The JSON string from bazel mod graph
   * @return A map of module keys to Module objects
   */
  fun parseModuleGraph(json: String): Map<String, Module> {
    val modules = mutableMapOf<String, Module>()

    try {
      val root = JsonParser.parseString(json).asJsonObject
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
