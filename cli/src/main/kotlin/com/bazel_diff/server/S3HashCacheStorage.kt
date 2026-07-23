package com.bazel_diff.server

import java.net.URI
import software.amazon.awssdk.core.exception.SdkException
import software.amazon.awssdk.core.sync.RequestBody
import software.amazon.awssdk.http.urlconnection.UrlConnectionHttpClient
import software.amazon.awssdk.regions.Region
import software.amazon.awssdk.services.s3.S3Client
import software.amazon.awssdk.services.s3.model.GetObjectRequest
import software.amazon.awssdk.services.s3.model.HeadObjectRequest
import software.amazon.awssdk.services.s3.model.NoSuchKeyException
import software.amazon.awssdk.services.s3.model.PutObjectRequest
import software.amazon.awssdk.services.s3.model.S3Exception

/**
 * [HashCacheStorage] backed by an S3 bucket -- the shared-cache backend the RFC (issue #29) calls
 * out so `serve` replicas behind a load balancer share one hash cache instead of each cold-hashing
 * every revision. Typically wrapped in a [TieredHashCacheStorage] with a
 * [LocalDiskHashCacheStorage] in front.
 *
 * Objects are stored as `<prefix><key>.json`, mirroring [LocalDiskHashCacheStorage]'s `<key>.json`
 * layout so entries are recognizable across backends. Cache keys (see `HashService.cacheKey`) are
 * `<sha>.<fingerprint>[.<digest>]`, already S3-key-safe.
 *
 * S3 failures are contained here rather than propagated: a read failure degrades to a cache miss
 * (the caller regenerates -- slower but correct, since an entry's content is deterministic for its
 * key) and a write failure is logged and dropped (whichever instance next generates the revision
 * re-publishes it). An S3 outage therefore slows the fleet but never fails requests. Concurrent
 * writers racing the same key are equally harmless: last-write-wins over identical content.
 *
 * Deliberately neither [MeasurableHashCacheStorage] (sizing a bucket in-process would be a paged
 * LIST per `/metrics` scrape) nor [PrunableHashCacheStorage] (retention belongs to a bucket
 * lifecycle policy) -- the local tier keeps both roles.
 */
class S3HashCacheStorage(
    private val s3: S3Client,
    private val bucket: String,
    prefix: String = "",
    private val warn: (String) -> Unit = { System.err.println("[Warn] $it") },
) : HashCacheStorage {
  private val prefix = normalizePrefix(prefix)

  /** Human-readable location (`s3://bucket/prefix/`) for logs and `/metrics`. */
  val location: String
    get() = "s3://$bucket/$prefix"

  private fun objectKey(key: String) = "$prefix$key.json"

  override fun get(key: String): ByteArray? =
      try {
        s3.getObjectAsBytes(GetObjectRequest.builder().bucket(bucket).key(objectKey(key)).build())
            .asByteArray()
      } catch (e: NoSuchKeyException) {
        null
      } catch (e: SdkException) {
        warn("S3 cache read of ${objectKey(key)} failed (treating as a miss): ${e.message}")
        null
      }

  override fun put(key: String, data: ByteArray) {
    try {
      s3.putObject(
          PutObjectRequest.builder()
              .bucket(bucket)
              .key(objectKey(key))
              .contentType("application/json")
              .build(),
          RequestBody.fromBytes(data))
    } catch (e: SdkException) {
      warn("S3 cache write of ${objectKey(key)} failed (entry not shared): ${e.message}")
    }
  }

  override fun contains(key: String): Boolean =
      try {
        s3.headObject(HeadObjectRequest.builder().bucket(bucket).key(objectKey(key)).build())
        true
      } catch (e: NoSuchKeyException) {
        false
      } catch (e: S3Exception) {
        // HeadObject reports a missing key as a bare 404 with no error body, which the SDK does
        // not always map to NoSuchKeyException.
        if (e.statusCode() != 404) {
          warn("S3 cache check of ${objectKey(key)} failed (treating as a miss): ${e.message}")
        }
        false
      } catch (e: SdkException) {
        warn("S3 cache check of ${objectKey(key)} failed (treating as a miss): ${e.message}")
        false
      }

  companion object {
    /**
     * Normalizes a user-supplied key prefix to either `""` or a slash-terminated path with no
     * leading slash, so `team/repo`, `/team/repo/` etc. all address the same objects.
     */
    fun normalizePrefix(prefix: String): String {
      val trimmed = prefix.trim('/')
      return if (trimmed.isEmpty()) "" else "$trimmed/"
    }

    /**
     * Builds the [S3Client] the `serve` wiring uses, explicitly on the JDK's UrlConnection HTTP
     * client (the SDK's Netty/Apache clients are excluded from the dependency tree to keep the
     * deploy jar lean). Region and credentials fall back to the SDK default provider chains (env
     * vars, profile, IRSA web identity, IMDS) when not given, so on EKS the pod's IAM role just
     * works. [endpoint] plus [forcePathStyle] point the client at an S3-compatible store (MinIO,
     * LocalStack) for testing.
     */
    fun buildClient(
        region: String? = null,
        endpoint: URI? = null,
        forcePathStyle: Boolean = false,
    ): S3Client {
      val builder = S3Client.builder().httpClientBuilder(UrlConnectionHttpClient.builder())
      region?.let { builder.region(Region.of(it)) }
      endpoint?.let { builder.endpointOverride(it) }
      if (forcePathStyle) builder.forcePathStyle(true)
      return builder.build()
    }
  }
}
