package com.bazel_diff.bazel

import com.google.gson.JsonObject
import com.google.gson.JsonParser

/**
 * Parses MODULE.bazel.lock files and detects changes to moduleExtensions.generatedRepoSpecs.
 *
 * Unlike `bazel mod graph --output=json`, which only reflects module version changes,
 * MODULE.bazel.lock records the actual repositories each module extension creates. This
 * allows detecting when an extension re-ran and resolved different repos — even if no
 * module version changed in MODULE.bazel.
 */
class ModuleLockFileParser {
    /**
     * Parses moduleExtensions.*.general.generatedRepoSpecs from a MODULE.bazel.lock JSON string.
     *
     * @return Map<extensionKey, Map<specKey, specJsonObject>>, empty on parse error.
     */
    fun parseGeneratedRepoSpecs(lockJson: String): Map<String, Map<String, JsonObject>> {
        return try {
            val root = JsonParser.parseString(lockJson).asJsonObject
            val result = mutableMapOf<String, Map<String, JsonObject>>()
            root.getAsJsonObject("moduleExtensions")?.entrySet()?.forEach { (extKey, extValue) ->
                val extObj = extValue?.asJsonObject ?: return@forEach
                // An extension entry contains one or more platform sections: "general",
                // "os:linux", "os:macos", "arch:x86_64", "os:linux,arch:x86_64", etc.
                // Collect generatedRepoSpecs from all sections — platform-specific extensions
                // (e.g. toolchain downloaders) never have a "general" key.
                val merged = mutableMapOf<String, JsonObject>()
                extObj.entrySet().forEach { (sectionKey, sectionValue) ->
                    sectionValue?.asJsonObject
                        ?.getAsJsonObject("generatedRepoSpecs")
                        ?.entrySet()
                        ?.forEach { (k, v) -> merged[k] = v.asJsonObject }
                }
                if (merged.isNotEmpty()) result[extKey] = merged
            }
            result
        } catch (e: Exception) {
            emptyMap()
        }
    }

    /**
     * Returns the canonical repo names (the `name` attribute inside each spec's `attributes`)
     * for repos that were added, removed, or had their spec changed between old and new.
     *
     * This is precise: only repos whose specs actually differ are returned, not all repos
     * managed by an extension just because its bzlTransitiveDigest changed.
     *
     * @param old generatedRepoSpecs from the starting revision
     * @param new generatedRepoSpecs from the final revision
     * @return Set of canonical Bazel repo names (e.g. "rules_jvm_external~6.3~maven~maven")
     */
    fun findChangedRepos(
        old: Map<String, Map<String, JsonObject>>,
        new: Map<String, Map<String, JsonObject>>
    ): Set<String> {
        val changed = mutableSetOf<String>()
        val allExtKeys = (old.keys + new.keys).toSet()
        for (extKey in allExtKeys) {
            val oldSpecs = old[extKey] ?: emptyMap()
            val newSpecs = new[extKey] ?: emptyMap()
            val allSpecKeys = (oldSpecs.keys + newSpecs.keys).toSet()
            for (specKey in allSpecKeys) {
                val oldSpec = oldSpecs[specKey]
                val newSpec = newSpecs[specKey]
                if (oldSpec?.toString() != newSpec?.toString()) {
                    // Prefer new spec for canonical name (e.g. renamed repos); fall back to old
                    val canonicalName = (newSpec ?: oldSpec)
                        ?.getAsJsonObject("attributes")
                        ?.get("name")?.asString
                    if (canonicalName != null) changed.add(canonicalName)
                }
            }
        }
        return changed
    }
}
