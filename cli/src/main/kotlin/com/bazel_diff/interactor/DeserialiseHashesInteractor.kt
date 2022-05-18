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
     */
    fun execute(file: File): Map<String, String> {
        val shape = object : TypeToken<Map<String, String>>() {}.type
        return gson.fromJson(FileReader(file), shape)
    }
}
