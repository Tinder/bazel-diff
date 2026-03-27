package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
import assertk.assertions.hasSize
import org.junit.Test

class ModuleLockFileParserTest {
    private val parser = ModuleLockFileParser()

    // Minimal lock file with one extension and two repo specs
    private val lockWithMavenExtension = """
        {
          "lockFileVersion": 26,
          "moduleExtensions": {
            "@@rules_jvm_external~6.3//:extensions.bzl%maven": {
              "general": {
                "bzlTransitiveDigest": "abc123==",
                "accumulatedFileDigests": {},
                "envVariables": {},
                "generatedRepoSpecs": {
                  "maven": {
                    "bzlFile": "@@rules_jvm_external~6.3//private/rules:coursier.bzl",
                    "ruleClassName": "pinned_coursier_fetch",
                    "attributes": {
                      "name": "rules_jvm_external~6.3~maven~maven",
                      "artifacts": ["{ \"group\": \"com.google.guava\", \"artifact\": \"guava\", \"version\": \"31.1-jre\" }"]
                    }
                  },
                  "com_google_guava_guava_31_1_jre": {
                    "bzlFile": "@@bazel_tools//tools/build_defs/repo:http.bzl",
                    "ruleClassName": "http_file",
                    "attributes": {
                      "name": "rules_jvm_external~6.3~maven~com_google_guava_guava_31_1_jre",
                      "sha256": "a42edc9cab792e39fe39bb94f3fca655ed157ff87a8af78e1d6ba5b07c4a00ab",
                      "urls": ["https://repo1.maven.org/maven2/com/google/guava/guava/31.1-jre/guava-31.1-jre.jar"]
                    }
                  }
                }
              }
            }
          }
        }
    """.trimIndent()

    @Test
    fun parseGeneratedRepoSpecs_withValidLock_extractsExtensionAndSpecs() {
        val result = parser.parseGeneratedRepoSpecs(lockWithMavenExtension)

        assertThat(result).hasSize(1)
        val extKey = "@@rules_jvm_external~6.3//:extensions.bzl%maven"
        assertThat(result.containsKey(extKey)).isEqualTo(true)
        assertThat(result[extKey]!!.keys)
            .containsExactlyInAnyOrder("maven", "com_google_guava_guava_31_1_jre")
    }

    @Test
    fun parseGeneratedRepoSpecs_withEmptyModuleExtensions_returnsEmptyMap() {
        val lock = """{"lockFileVersion": 26, "moduleExtensions": {}}"""
        assertThat(parser.parseGeneratedRepoSpecs(lock)).isEmpty()
    }

    @Test
    fun parseGeneratedRepoSpecs_withMissingModuleExtensions_returnsEmptyMap() {
        val lock = """{"lockFileVersion": 26}"""
        assertThat(parser.parseGeneratedRepoSpecs(lock)).isEmpty()
    }

    @Test
    fun parseGeneratedRepoSpecs_withInvalidJson_returnsEmptyMap() {
        assertThat(parser.parseGeneratedRepoSpecs("{ invalid json")).isEmpty()
    }

    @Test
    fun findChangedRepos_withNoChanges_returnsEmptySet() {
        val specs = parser.parseGeneratedRepoSpecs(lockWithMavenExtension)
        assertThat(parser.findChangedRepos(specs, specs)).isEmpty()
    }

    @Test
    fun findChangedRepos_withAddedRepo_returnsCanonicalNameOfNewRepo() {
        val oldSpecs = parser.parseGeneratedRepoSpecs(lockWithMavenExtension)
        val lockWithExtraRepo = lockWithMavenExtension.replace(
            """"com_google_guava_guava_31_1_jre"""",
            """"com_google_guava_guava_32_0_0_jre""""
        ).replace(
            """"name": "rules_jvm_external~6.3~maven~com_google_guava_guava_31_1_jre"""",
            """"name": "rules_jvm_external~6.3~maven~com_google_guava_guava_32_0_0_jre""""
        ).replace(
            """"sha256": "a42edc9cab792e39fe39bb94f3fca655ed157ff87a8af78e1d6ba5b07c4a00ab"""",
            """"sha256": "NEWSHA256""""
        ).replace(
            "31.1-jre/guava-31.1-jre.jar",
            "32.0.0-jre/guava-32.0.0-jre.jar"
        )
        val newSpecs = parser.parseGeneratedRepoSpecs(lockWithExtraRepo)

        val changed = parser.findChangedRepos(oldSpecs, newSpecs)

        // Old repo removed, new repo added
        assertThat(changed).containsExactlyInAnyOrder(
            "rules_jvm_external~6.3~maven~com_google_guava_guava_31_1_jre",
            "rules_jvm_external~6.3~maven~com_google_guava_guava_32_0_0_jre"
        )
    }

    @Test
    fun findChangedRepos_withChangedRepoAttribute_returnsCanonicalName() {
        val oldSpecs = parser.parseGeneratedRepoSpecs(lockWithMavenExtension)
        // Change only the sha256 — same repo key, different content
        val lockWithDifferentSha = lockWithMavenExtension.replace(
            "a42edc9cab792e39fe39bb94f3fca655ed157ff87a8af78e1d6ba5b07c4a00ab",
            "DIFFERENT_SHA256_VALUE_HERE_0000000000000000000000000000000000000000"
        )
        val newSpecs = parser.parseGeneratedRepoSpecs(lockWithDifferentSha)

        val changed = parser.findChangedRepos(oldSpecs, newSpecs)

        assertThat(changed).containsExactlyInAnyOrder(
            "rules_jvm_external~6.3~maven~com_google_guava_guava_31_1_jre"
        )
    }

    @Test
    fun findChangedRepos_withChangedAggregateRepoArtifacts_returnsAggregateCanonicalName() {
        val oldSpecs = parser.parseGeneratedRepoSpecs(lockWithMavenExtension)
        val lockWithNewArtifacts = lockWithMavenExtension.replace(
            "31.1-jre",
            "32.0.0-jre"
        )
        val newSpecs = parser.parseGeneratedRepoSpecs(lockWithNewArtifacts)

        val changed = parser.findChangedRepos(oldSpecs, newSpecs)

        // The "maven" aggregate repo spec changed (artifacts list changed)
        assertThat(changed.contains("rules_jvm_external~6.3~maven~maven")).isEqualTo(true)
    }

    @Test
    fun findChangedRepos_withEmptyOldSpecs_returnsAllNewRepoCanonicalNames() {
        val newSpecs = parser.parseGeneratedRepoSpecs(lockWithMavenExtension)

        val changed = parser.findChangedRepos(emptyMap(), newSpecs)

        assertThat(changed).containsExactlyInAnyOrder(
            "rules_jvm_external~6.3~maven~maven",
            "rules_jvm_external~6.3~maven~com_google_guava_guava_31_1_jre"
        )
    }

    @Test
    fun findChangedRepos_withEmptyNewSpecs_returnsAllOldRepoCanonicalNames() {
        val oldSpecs = parser.parseGeneratedRepoSpecs(lockWithMavenExtension)

        val changed = parser.findChangedRepos(oldSpecs, emptyMap())

        assertThat(changed).containsExactlyInAnyOrder(
            "rules_jvm_external~6.3~maven~maven",
            "rules_jvm_external~6.3~maven~com_google_guava_guava_31_1_jre"
        )
    }

    @Test
    fun parseGeneratedRepoSpecs_withOsSpecificSections_extractsAllSections() {
        // Extensions like toolchain downloaders use "os:linux", "os:macos" etc. instead of "general"
        val lockWithOsSpecificExtension = """
            {
              "lockFileVersion": 10,
              "moduleExtensions": {
                "@@rules_go~//:extensions.bzl%go_sdk": {
                  "os:linux": {
                    "bzlTransitiveDigest": "abc==",
                    "generatedRepoSpecs": {
                      "go_sdk_linux": {
                        "bzlFile": "@@rules_go//go:def.bzl",
                        "ruleClassName": "go_sdk",
                        "attributes": {
                          "name": "rules_go~~go_sdk~go_sdk_linux",
                          "version": "1.21.0"
                        }
                      }
                    }
                  },
                  "os:macos": {
                    "bzlTransitiveDigest": "abc==",
                    "generatedRepoSpecs": {
                      "go_sdk_macos": {
                        "bzlFile": "@@rules_go//go:def.bzl",
                        "ruleClassName": "go_sdk",
                        "attributes": {
                          "name": "rules_go~~go_sdk~go_sdk_macos",
                          "version": "1.21.0"
                        }
                      }
                    }
                  }
                }
              }
            }
        """.trimIndent()

        val result = parser.parseGeneratedRepoSpecs(lockWithOsSpecificExtension)

        assertThat(result).hasSize(1)
        val extKey = "@@rules_go~//:extensions.bzl%go_sdk"
        assertThat(result.containsKey(extKey)).isEqualTo(true)
        assertThat(result[extKey]!!.keys).containsExactlyInAnyOrder("go_sdk_linux", "go_sdk_macos")
    }

    @Test
    fun findChangedRepos_withOsSpecificSectionChanged_returnsCanonicalName() {
        val lockV1 = """
            {
              "lockFileVersion": 10,
              "moduleExtensions": {
                "@@rules_go~//:extensions.bzl%go_sdk": {
                  "os:linux": {
                    "bzlTransitiveDigest": "abc==",
                    "generatedRepoSpecs": {
                      "go_sdk_linux": {
                        "bzlFile": "@@rules_go//go:def.bzl",
                        "ruleClassName": "go_sdk",
                        "attributes": { "name": "rules_go~~go_sdk~go_sdk_linux", "version": "1.21.0" }
                      }
                    }
                  }
                }
              }
            }
        """.trimIndent()
        val lockV2 = lockV1.replace("\"version\": \"1.21.0\"", "\"version\": \"1.22.0\"")

        val oldSpecs = parser.parseGeneratedRepoSpecs(lockV1)
        val newSpecs = parser.parseGeneratedRepoSpecs(lockV2)

        assertThat(parser.findChangedRepos(oldSpecs, newSpecs))
            .containsExactlyInAnyOrder("rules_go~~go_sdk~go_sdk_linux")
    }
}
