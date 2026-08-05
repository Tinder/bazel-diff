package com.bazel_diff.server

/**
 * Two-tier [HashCacheStorage]: a fast local tier backed by a shared remote tier (the RFC issue #29
 * S3 backend), so `serve` replicas behind a load balancer share generated hashes while hot
 * revisions are still served from local disk.
 *
 * Reads hit [local] first; a remote hit is backfilled into [local] so subsequent reads (and the
 * local LRU recency that pruning relies on) stay local. Writes go to both tiers, publishing every
 * generated revision to the fleet.
 *
 * The measurable/prunable surface is the *local* tier only: `--cacheMax*` pruning bounds local disk
 * usage and `/metrics` reports the local footprint. Remote retention belongs to the backend (e.g. a
 * bucket lifecycle policy) -- see [S3HashCacheStorage]. A pruned-locally entry that still exists
 * remotely is simply re-backfilled on next use.
 */
class TieredHashCacheStorage<L>(
    private val local: L,
    private val remote: HashCacheStorage,
) : MeasurableHashCacheStorage, PrunableHashCacheStorage where
L : MeasurableHashCacheStorage,
L : PrunableHashCacheStorage {
  override fun get(key: String): ByteArray? {
    local.get(key)?.let {
      return it
    }
    return remote.get(key)?.also { local.put(key, it) }
  }

  override fun put(key: String, data: ByteArray) {
    local.put(key, data)
    remote.put(key, data)
  }

  override fun contains(key: String): Boolean = local.contains(key) || remote.contains(key)

  override fun stats(): CacheStorageStats = local.stats()

  override fun prune(limits: CachePruneLimits): CachePruneResult = local.prune(limits)
}
