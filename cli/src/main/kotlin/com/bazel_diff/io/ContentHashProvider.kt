package com.bazel_diff.io

import com.bazel_diff.interactor.DeserialiseHashesInteractor
import java.io.File

class ContentHashProvider(file: File?) {
    // filename relative to workspace -> content hash of the file
    val filenameToHash: Map<String, String>? = if (file == null) null else readJson(file)

    private fun readJson(file: File): Map<String, String> {
        val deserialiser = DeserialiseHashesInteractor()
        return deserialiser.execute(file)
    }
}
