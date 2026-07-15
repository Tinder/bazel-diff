package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.hasSize
import assertk.assertions.isEmpty
import assertk.assertions.isEqualTo
import assertk.assertions.isFalse
import assertk.assertions.isNull
import assertk.assertions.isTrue
import java.net.URI
import java.nio.charset.StandardCharsets
import org.junit.Test
import org.mockito.kotlin.any
import org.mockito.kotlin.argumentCaptor
import org.mockito.kotlin.doAnswer
import org.mockito.kotlin.doReturn
import org.mockito.kotlin.doThrow
import org.mockito.kotlin.mock
import org.mockito.kotlin.whenever
import software.amazon.awssdk.core.ResponseBytes
import software.amazon.awssdk.core.exception.SdkClientException
import software.amazon.awssdk.core.sync.RequestBody
import software.amazon.awssdk.services.s3.S3Client
import software.amazon.awssdk.services.s3.model.GetObjectRequest
import software.amazon.awssdk.services.s3.model.GetObjectResponse
import software.amazon.awssdk.services.s3.model.HeadObjectRequest
import software.amazon.awssdk.services.s3.model.HeadObjectResponse
import software.amazon.awssdk.services.s3.model.NoSuchKeyException
import software.amazon.awssdk.services.s3.model.PutObjectRequest
import software.amazon.awssdk.services.s3.model.PutObjectResponse
import software.amazon.awssdk.services.s3.model.S3Exception

class S3HashCacheStorageTest {
  private val s3: S3Client = mock()
  private val warnings = mutableListOf<String>()

  private fun storage(prefix: String = "") =
      S3HashCacheStorage(s3, "bucket", prefix, warn = { warnings += it })

  private fun bytes(s: String) = s.toByteArray(StandardCharsets.UTF_8)

  private fun stubGetReturning(data: ByteArray) {
    whenever(s3.getObjectAsBytes(any<GetObjectRequest>()))
        .doReturn(ResponseBytes.fromByteArray(GetObjectResponse.builder().build(), data))
  }

  @Test
  fun getReturnsObjectBytesUnderThePrefixedKey() {
    stubGetReturning(bytes("hello"))

    val result = storage(prefix = "team/repo").get("sha.fp")

    assertThat(String(result!!, StandardCharsets.UTF_8)).isEqualTo("hello")
    val captor = argumentCaptor<GetObjectRequest>()
    org.mockito.kotlin.verify(s3).getObjectAsBytes(captor.capture())
    assertThat(captor.firstValue.bucket()).isEqualTo("bucket")
    assertThat(captor.firstValue.key()).isEqualTo("team/repo/sha.fp.json")
  }

  @Test
  fun getMissReturnsNullWithoutWarning() {
    whenever(s3.getObjectAsBytes(any<GetObjectRequest>()))
        .doThrow(NoSuchKeyException.builder().build())

    assertThat(storage().get("absent")).isNull()
    assertThat(warnings).isEmpty()
  }

  @Test
  fun getErrorDegradesToMissWithWarning() {
    // An S3 outage must slow the service (regenerate), never fail the request.
    whenever(s3.getObjectAsBytes(any<GetObjectRequest>()))
        .doThrow(SdkClientException.create("connection refused"))

    assertThat(storage().get("sha.fp")).isNull()
    assertThat(warnings).hasSize(1)
    assertThat(warnings.first()).contains("treating as a miss")
  }

  @Test
  fun putUploadsJsonObjectUnderThePrefixedKey() {
    val requestCaptor = argumentCaptor<PutObjectRequest>()
    val bodyCaptor = argumentCaptor<RequestBody>()
    whenever(s3.putObject(requestCaptor.capture(), bodyCaptor.capture()))
        .doReturn(PutObjectResponse.builder().build())

    storage(prefix = "p").put("sha.fp", bytes("data"))

    assertThat(requestCaptor.firstValue.bucket()).isEqualTo("bucket")
    assertThat(requestCaptor.firstValue.key()).isEqualTo("p/sha.fp.json")
    assertThat(requestCaptor.firstValue.contentType()).isEqualTo("application/json")
    val uploaded = bodyCaptor.firstValue.contentStreamProvider().newStream().readBytes()
    assertThat(String(uploaded, StandardCharsets.UTF_8)).isEqualTo("data")
  }

  @Test
  fun putErrorIsSwallowedWithWarning() {
    // A failed publish leaves the entry local-only; the next instance to generate re-publishes.
    whenever(s3.putObject(any<PutObjectRequest>(), any<RequestBody>()))
        .doThrow(SdkClientException.create("boom"))

    storage().put("sha.fp", bytes("data"))

    assertThat(warnings).hasSize(1)
    assertThat(warnings.first()).contains("not shared")
  }

  @Test
  fun containsIsTrueWhenHeadSucceeds() {
    whenever(s3.headObject(any<HeadObjectRequest>())).doReturn(HeadObjectResponse.builder().build())

    assertThat(storage().contains("sha.fp")).isTrue()
  }

  @Test
  fun containsIsFalseOnNoSuchKey() {
    whenever(s3.headObject(any<HeadObjectRequest>())).doThrow(NoSuchKeyException.builder().build())

    assertThat(storage().contains("absent")).isFalse()
    assertThat(warnings).isEmpty()
  }

  @Test
  fun containsIsFalseOnBare404WithoutWarning() {
    // HeadObject reports a missing key as a bare 404 with no error body, which the SDK does not
    // always map to NoSuchKeyException.
    whenever(s3.headObject(any<HeadObjectRequest>())).doAnswer {
      throw S3Exception.builder().statusCode(404).build()
    }

    assertThat(storage().contains("absent")).isFalse()
    assertThat(warnings).isEmpty()
  }

  @Test
  fun containsIsFalseOnServiceErrorWithWarning() {
    whenever(s3.headObject(any<HeadObjectRequest>())).doAnswer {
      throw S3Exception.builder().statusCode(500).message("oops").build()
    }

    assertThat(storage().contains("sha.fp")).isFalse()
    assertThat(warnings).hasSize(1)
  }

  @Test
  fun containsIsFalseOnClientErrorWithWarning() {
    whenever(s3.headObject(any<HeadObjectRequest>()))
        .doThrow(SdkClientException.create("no route to host"))

    assertThat(storage().contains("sha.fp")).isFalse()
    assertThat(warnings).hasSize(1)
  }

  @Test
  fun prefixIsNormalizedToSlashTerminatedWithNoLeadingSlash() {
    assertThat(S3HashCacheStorage.normalizePrefix("")).isEqualTo("")
    assertThat(S3HashCacheStorage.normalizePrefix("/")).isEqualTo("")
    assertThat(S3HashCacheStorage.normalizePrefix("a")).isEqualTo("a/")
    assertThat(S3HashCacheStorage.normalizePrefix("/a/b/")).isEqualTo("a/b/")
    assertThat(S3HashCacheStorage.normalizePrefix("a/b")).isEqualTo("a/b/")
  }

  @Test
  fun locationDescribesBucketAndNormalizedPrefix() {
    assertThat(storage(prefix = "/team/repo/").location).isEqualTo("s3://bucket/team/repo/")
    assertThat(storage().location).isEqualTo("s3://bucket/")
  }

  @Test
  fun buildClientAcceptsRegionEndpointAndPathStyle() {
    // Building a client performs no network I/O and resolves no credentials, so the factory the
    // serve wiring uses can be exercised directly, including the S3-compatible-store shape.
    S3HashCacheStorage.buildClient(
            region = "us-east-1", endpoint = URI("http://localhost:9000"), forcePathStyle = true)
        .use { client -> assertThat(client.serviceName()).isEqualTo("s3") }
    S3HashCacheStorage.buildClient(region = "us-west-2").use { client ->
      assertThat(client.serviceName()).isEqualTo("s3")
    }
  }
}
