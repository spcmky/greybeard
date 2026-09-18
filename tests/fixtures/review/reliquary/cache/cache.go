// Package cache provides unified caching abstractions for media streaming.
//
// The Cache interface is used by both sshstream (for remote file chunks) and
// the segment generator (for HLS segments). CacheManager coordinates named
// cache instances with runtime configuration.
package cache

import (
	"sync/atomic"
)

// Cache is the unified caching interface used by all components.
type Cache interface {
	// Get retrieves cached data. Returns data and true if found, nil and false otherwise.
	Get(key string) ([]byte, bool)

	// Put stores data in the cache. May evict other entries if at capacity.
	Put(key string, data []byte)

	// Delete removes an entry from the cache.
	Delete(key string)

	// DeletePrefix removes all entries with keys starting with the given prefix.
	// Used by segment generator's ClearCache(fileID) to remove all segments for a file.
	DeletePrefix(prefix string) error

	// Stats returns cache statistics.
	Stats() Stats
}

// Resizable is implemented by caches that support non-blocking size changes.
type Resizable interface {
	// Resize adjusts cache capacity without blocking ongoing operations.
	// For memory caches: triggers eviction in background if shrinking.
	// For filesystem caches: updates limit, eviction happens lazily on next Put.
	Resize(newMaxBytes int64)
}

// Closeable is implemented by caches that need cleanup on shutdown.
type Closeable interface {
	Close() error
}

// Durable is implemented by caches that can guarantee writes persist or return an error.
type Durable interface {
	DurablePut(key string, data []byte) error
}

type ownershipCache interface {
	PutOwned(key string, data []byte)
}

// DurablePut writes through Durable.DurablePut if supported, otherwise falls back to Put.
func DurablePut(c Cache, key string, data []byte) error {
	if d, ok := c.(Durable); ok {
		return d.DurablePut(key, data)
	}
	c.Put(key, data)
	return nil
}

// PutOwned permits the cache to retain data, so callers must relinquish mutation after the call.
func PutOwned(c Cache, key string, data []byte) {
	if owned, ok := c.(ownershipCache); ok {
		owned.PutOwned(key, data)
		return
	}
	c.Put(key, data)
}

// AccessCounter is implemented by caches that track per-key access counts.
type AccessCounter interface {
	AccessCount(key string) int64
}

// Stats contains cache performance metrics.
type Stats struct {
	Hits        int64 `json:"hits"`
	Misses      int64 `json:"misses"`
	Evictions   int64 `json:"evictions"`
	BytesCached int64 `json:"bytesCached"`
	MaxCapacity int64 `json:"maxCapacity"`
	ItemCount   int   `json:"itemCount"`
}

// HitRate returns the cache hit rate as a percentage (0-100).
func (s Stats) HitRate() float64 {
	total := s.Hits + s.Misses
	if total == 0 {
		return 0
	}
	return float64(s.Hits) / float64(total) * 100
}

// Utilization returns the cache utilization as a percentage (0-100).
func (s Stats) Utilization() float64 {
	if s.MaxCapacity == 0 {
		return 0
	}
	return float64(s.BytesCached) / float64(s.MaxCapacity) * 100
}

// statsCollector provides atomic counters for cache statistics.
type statsCollector struct {
	hits      atomic.Int64
	misses    atomic.Int64
	evictions atomic.Int64
}

func (s *statsCollector) hit()           { s.hits.Add(1) }
func (s *statsCollector) miss()          { s.misses.Add(1) }
func (s *statsCollector) evict()         { s.evictions.Add(1) }
func (s *statsCollector) evictN(n int64) { s.evictions.Add(n) }

func (s *statsCollector) snapshot() (hits, misses, evictions int64) {
	return s.hits.Load(), s.misses.Load(), s.evictions.Load()
}
