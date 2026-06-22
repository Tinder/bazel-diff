package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import org.junit.Test

class FingerprintInteractorTest {
  private val interactor = FingerprintInteractor()

  private fun inputs(
      bazelDiffVersion: String = "26.0.1",
      bazelVersion: String = "8.5.1",
      moduleLockContent: ByteArray? = "lock-contents".toByteArray(),
      bazelrcContents: Map<String, ByteArray> = mapOf(".bazelrc" to "common --x".toByteArray()),
      flags: Map<String, String> = mapOf("useCquery" to "false"),
  ) =
      FingerprintInputs(
          bazelDiffVersion = bazelDiffVersion,
          bazelVersion = bazelVersion,
          moduleLockContent = moduleLockContent,
          bazelrcContents = bazelrcContents,
          flags = flags,
      )

  @Test
  fun deterministic_sameInputsSameFingerprint() {
    val a = interactor.compute(inputs())
    val b = interactor.compute(inputs())
    assertThat(a.fingerprint).isEqualTo(b.fingerprint)
  }

  @Test
  fun flagMapOrderDoesNotMatter() {
    val a =
        interactor.compute(
            inputs(flags = linkedMapOf("useCquery" to "true", "keepGoing" to "false")))
    val b =
        interactor.compute(
            inputs(flags = linkedMapOf("keepGoing" to "false", "useCquery" to "true")))
    assertThat(a.fingerprint).isEqualTo(b.fingerprint)
  }

  @Test
  fun bazelrcOrderDoesNotMatter() {
    val a =
        interactor.compute(
            inputs(
                bazelrcContents =
                    linkedMapOf(
                        ".bazelrc" to "a".toByteArray(), "ci.bazelrc" to "b".toByteArray())))
    val b =
        interactor.compute(
            inputs(
                bazelrcContents =
                    linkedMapOf(
                        "ci.bazelrc" to "b".toByteArray(), ".bazelrc" to "a".toByteArray())))
    assertThat(a.fingerprint).isEqualTo(b.fingerprint)
  }

  @Test
  fun bazelVersionChangeChangesFingerprint() {
    val a = interactor.compute(inputs(bazelVersion = "8.5.1"))
    val b = interactor.compute(inputs(bazelVersion = "8.6.0"))
    assertThat(a.fingerprint).isNotEqualTo(b.fingerprint)
  }

  @Test
  fun bazelDiffVersionChangeChangesFingerprint() {
    val a = interactor.compute(inputs(bazelDiffVersion = "26.0.1"))
    val b = interactor.compute(inputs(bazelDiffVersion = "27.0.0"))
    assertThat(a.fingerprint).isNotEqualTo(b.fingerprint)
  }

  @Test
  fun moduleLockChangeChangesFingerprint() {
    val a = interactor.compute(inputs(moduleLockContent = "v1".toByteArray()))
    val b = interactor.compute(inputs(moduleLockContent = "v2".toByteArray()))
    assertThat(a.fingerprint).isNotEqualTo(b.fingerprint)
  }

  @Test
  fun missingModuleLockIsDistinctFromEmpty() {
    val absent = interactor.compute(inputs(moduleLockContent = null))
    val empty = interactor.compute(inputs(moduleLockContent = ByteArray(0)))
    // null (no lockfile) and a present-but-empty lockfile must not collide.
    assertThat(absent.components["moduleLock"]).isNotEqualTo(empty.components["moduleLock"])
  }

  @Test
  fun bazelrcChangeChangesFingerprint() {
    val a = interactor.compute(inputs(bazelrcContents = mapOf(".bazelrc" to "x".toByteArray())))
    val b = interactor.compute(inputs(bazelrcContents = mapOf(".bazelrc" to "y".toByteArray())))
    assertThat(a.fingerprint).isNotEqualTo(b.fingerprint)
  }

  @Test
  fun flagChangeChangesFingerprint() {
    val a = interactor.compute(inputs(flags = mapOf("useCquery" to "false")))
    val b = interactor.compute(inputs(flags = mapOf("useCquery" to "true")))
    assertThat(a.fingerprint).isNotEqualTo(b.fingerprint)
  }

  @Test
  fun exposesPerComponentHashes() {
    val r = interactor.compute(inputs())
    assertThat(r.components.keys)
        .isEqualTo(setOf("bazelDiffVersion", "bazelVersion", "moduleLock", "bazelrc", "flags"))
  }
}
