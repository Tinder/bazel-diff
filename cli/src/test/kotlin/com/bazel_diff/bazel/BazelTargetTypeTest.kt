package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.hasSize
import assertk.assertions.isEqualTo
import org.junit.Test

class BazelTargetTypeTest {
  @Test
  fun enumDeclaresExpectedValues() {
    // Reading every constant forces the JVM to initialise the whole enum, which
    // is what gives this otherwise-trivial declaration any line coverage.
    val all = BazelTargetType.entries
    assertThat(all).hasSize(6)
    assertThat(all)
        .containsExactlyInAnyOrder(
            BazelTargetType.RULE,
            BazelTargetType.SOURCE_FILE,
            BazelTargetType.GENERATED_FILE,
            BazelTargetType.PACKAGE_GROUP,
            BazelTargetType.ENVIRONMENT_GROUP,
            BazelTargetType.UNKNOWN,
        )
  }

  @Test
  fun valueOfRoundTrip() {
    for (t in BazelTargetType.entries) {
      assertThat(BazelTargetType.valueOf(t.name)).isEqualTo(t)
    }
  }
}
