package com.bazel_diff.interactor

import com.bazel_diff.hash.TargetHash
import com.google.gson.Gson
import com.google.gson.JsonObject
import com.google.gson.reflect.TypeToken
import java.io.File
import java.io.FileReader
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

data class HashFileData(
    val hashes: Map<String, TargetHash>,
    val moduleGraphJson: String?
)

class DeserialiseHashesInteractor : KoinComponent {
  private val gson: Gson by inject()

  /**
   * @param file path to file that has been pre-validated
   * @param targetTypes the target types to filter. If null, all targets will be returned
   * @return HashFileData containing hashes and optional module graph JSON
   */
  fun executeTargetHashWithMetadata(file: File): HashFileData {
    val jsonObject = gson.fromJson(FileReader(file), JsonObject::class.java)

    // Check if this is the new format with metadata
    if (jsonObject.has("hashes") && jsonObject.has("metadata")) {
      // New format
      val hashesShape = object : TypeToken<Map<String, String>>() {}.type
      val hashesMap: Map<String, String> = gson.fromJson(jsonObject.get("hashes"), hashesShape)
      val hashes = hashesMap.mapValues { TargetHash.fromJson(it.value) }

      val moduleGraphJson = jsonObject.getAsJsonObject("metadata")
          ?.get("moduleGraphJson")
          ?.asString

      return HashFileData(hashes, moduleGraphJson)
    } else {
      // Legacy format - just a flat map of hashes
      val shape = object : TypeToken<Map<String, String>>() {}.type
      val result: Map<String, String> = gson.fromJson(jsonObject, shape)
      val hashes = result.mapValues { TargetHash.fromJson(it.value) }
      return HashFileData(hashes, null)
    }
  }

  /**
   * @param file path to file that has been pre-validated
   * @param targetTypes the target types to filter. If null, all targets will be returned
   */
  fun executeTargetHash(file: File): Map<String, TargetHash> {
    return executeTargetHashWithMetadata(file).hashes
  }

  /**
   * Deserializes hashes from the given file.
   *
   * Used for deserializing the content hashes of files, which are represented as a map of file
   * paths to their content hashes.
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
