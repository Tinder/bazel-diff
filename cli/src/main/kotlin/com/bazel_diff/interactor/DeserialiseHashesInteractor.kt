package com.bazel_diff.interactor

import com.google.gson.Gson
import com.google.gson.reflect.TypeToken
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.io.File
import java.io.FileReader

class DeserialiseHashesInteractor : KoinComponent {
    private val gson: Gson by inject()

    /**
     * @param file path to file that has been pre-validated
     * @param targetTypes the target types to filter. If null, all targets will be returned
     */
    fun execute(file: File, targetTypes: Set<String>? = null): Map<String, String> {
        val shape = object : TypeToken<Map<String, String>>() {}.type
        val result: Map<String, String> = gson.fromJson(FileReader(file), shape)
        if (targetTypes == null) {
            return result.mapValues { it.value.substringAfter("#") }
        } else {
            val prefixes = targetTypes.map { "${it}#" }.toSet()
            return result.filter { entry ->
                if (entry.value.contains("#")) {
                    prefixes.any { entry.value.startsWith(it) }
                } else {
                    throw IllegalStateException("No type info found in ${file}, please re-generate the JSON with --includeTypeTarget!")
                }
            }.mapValues { it.value.substringAfter("#") }
        }
    }
}
