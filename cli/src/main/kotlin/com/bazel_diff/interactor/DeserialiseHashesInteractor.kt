package com.bazel_diff.interactor

import com.google.gson.Gson
import com.google.gson.reflect.TypeToken
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.io.File
import java.io.FileReader
import com.bazel_diff.hash.TargetHash

class DeserialiseHashesInteractor : KoinComponent {
    private val gson: Gson by inject()

    /**
     * @param file path to file that has been pre-validated
     * @param targetTypes the target types to filter. If null, all targets will be returned
     */
    fun executeTargetHash(file: File): Map<String, TargetHash> {
        val shape = object : TypeToken<Map<String, String>>() {}.type
        val result: Map<String, String> = gson.fromJson(FileReader(file), shape)
        return result.mapValues { TargetHash.fromJson(it.value) }
    }

    /**
     * Deserializes hashes from the given file.
     * 
     * Used for deserializing the content hashes of files, which are represented as
     * a map of file paths to their content hashes.
     *
     * @param file The path to the file that has been pre-validated.
     * @return A map containing the deserialized hashes.
     */
    fun executeSimple(file: File): Map<String, String> {
        val shape = object : TypeToken<Map<String, String>>() {}.type
        return gson.fromJson(FileReader(file), shape)
    }

    fun deserializeDeps(file: File): Map<String, List<String>> {
        val shape = object : TypeToken<Map<String, List<String>>>() {}.type
        return gson.fromJson(FileReader(file), shape)
    }
}
