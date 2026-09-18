package cache

import (
	"bytes"
	"container/list"
	"strings"
	"sync"
	"time"
)

type LFUCache struct {
	mu           sync.RWMutex
	maxBytes     int64
	currentBytes int64

	items    map[string]*lfuItem
	freqList list.List

	stats statsCollector
}

type lfuItem struct {
	key        string
	data       []byte
	size       int64
	cost       int64
	bucket     *list.Element
	listElem   *list.Element
	createdAt  time.Time
	lastAccess time.Time
}

type frequencyNode struct {
	frequency int64
	items     list.List
}

// NewLFUCache creates a new LFU cache with the specified capacity.
func NewLFUCache(maxBytes int64) (*LFUCache, error) {
	return &LFUCache{
		maxBytes: maxBytes,
		items:    make(map[string]*lfuItem),
	}, nil
}

// Get retrieves an item from the cache and updates its frequency.
func (c *LFUCache) Get(key string) ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()

	item, exists := c.items[key]
	if !exists {
		c.stats.miss()
		return nil, false
	}

	c.stats.hit()
	item.lastAccess = time.Now()

	// Update frequency
	c.updateFrequency(item)

	// Callers must treat returned data as read-only. Eviction only drops the cache reference.
	return item.data, true
}

// Put adds or updates an item in the cache.
func (c *LFUCache) Put(key string, data []byte) {
	c.mu.RLock()
	tooLarge := int64(len(data)) > c.maxBytes
	c.mu.RUnlock()
	if tooLarge {
		return
	}
	c.put(key, bytes.Clone(data), int64(len(data)))
}

func (c *LFUCache) PutOwned(key string, data []byte) {
	c.put(key, data, int64(cap(data)))
}

func (c *LFUCache) put(key string, data []byte, dataCost int64) {
	c.mu.Lock()
	defer c.mu.Unlock()

	dataSize := int64(len(data))
	if dataCost > c.maxBytes {
		// Item is too large for cache
		return
	}

	now := time.Now()
	item, exists := c.items[key]
	if exists {
		c.currentBytes -= item.cost
		c.updateFrequency(item)
	} else {
		item = &lfuItem{key: key, createdAt: now}
		bucket := c.freqList.Front()
		if bucket == nil || bucket.Value.(*frequencyNode).frequency != 1 {
			bucket = c.freqList.PushFront(&frequencyNode{frequency: 1})
		}
		c.addToFrequencyList(item, bucket)
		c.items[key] = item
	}
	defer c.enforceCapacity(item)
	item.data = data
	item.size = dataSize
	item.cost = dataCost
	item.lastAccess = now
	c.currentBytes += dataCost
}

// Delete removes an entry from the cache.
func (c *LFUCache) Delete(key string) {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.removeItem(key)
}

// DeletePrefix removes all entries with keys starting with the given prefix.
func (c *LFUCache) DeletePrefix(prefix string) error {
	c.mu.Lock()
	defer c.mu.Unlock()

	// Collect keys to delete (can't modify map while iterating)
	var keysToDelete []string
	for key := range c.items {
		if strings.HasPrefix(key, prefix) {
			keysToDelete = append(keysToDelete, key)
		}
	}

	for _, key := range keysToDelete {
		c.removeItem(key)
	}

	return nil
}

// Stats returns cache statistics.
func (c *LFUCache) Stats() Stats {
	c.mu.RLock()
	itemCount := len(c.items)
	currentBytes := c.currentBytes
	maxBytes := c.maxBytes
	c.mu.RUnlock()

	hits, misses, evictions := c.stats.snapshot()

	return Stats{
		Hits:        hits,
		Misses:      misses,
		Evictions:   evictions,
		BytesCached: currentBytes,
		MaxCapacity: maxBytes,
		ItemCount:   itemCount,
	}
}

// Resize adjusts cache capacity without blocking ongoing operations.
func (c *LFUCache) Resize(newMaxBytes int64) {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.maxBytes = newMaxBytes

	c.enforceCapacity(nil)
}

// List returns entries for inspection (implements Inspector).
func (c *LFUCache) List(prefix string, limit int, cursor string) (*ListResult, error) {
	c.mu.RLock()
	entries := make([]Entry, 0, len(c.items))
	for key, item := range c.items {
		if !strings.HasPrefix(key, prefix) {
			continue
		}
		entries = append(entries, Entry{
			Key:         key,
			Size:        item.size,
			CreatedAt:   item.createdAt,
			LastAccess:  item.lastAccess,
			AccessCount: item.bucket.Value.(*frequencyNode).frequency,
		})
	}
	c.mu.RUnlock()
	return paginateEntries(entries, limit, cursor)
}

// Clear removes all items from the cache.
func (c *LFUCache) Clear() {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.items = make(map[string]*lfuItem)
	c.freqList.Init()
	c.currentBytes = 0
}

func (c *LFUCache) updateFrequency(item *lfuItem) {
	bucket := item.bucket
	node := bucket.Value.(*frequencyNode)
	next := bucket.Next()
	if next == nil || next.Value.(*frequencyNode).frequency != node.frequency+1 {
		next = c.freqList.InsertAfter(&frequencyNode{frequency: node.frequency + 1}, bucket)
	}
	node.items.Remove(item.listElem)
	if node.items.Len() == 0 {
		c.freqList.Remove(bucket)
	}
	c.addToFrequencyList(item, next)
}

func (c *LFUCache) addToFrequencyList(item *lfuItem, bucket *list.Element) {
	item.bucket = bucket
	item.listElem = bucket.Value.(*frequencyNode).items.PushBack(item)
}

func (c *LFUCache) enforceCapacity(keep *lfuItem) {
	for c.currentBytes > c.maxBytes && len(c.items) > 0 {
		if !c.evictLFU(keep) {
			panic("cache cannot evict to capacity")
		}
	}
}

func (c *LFUCache) evictLFU(keep *lfuItem) bool {
	bucket := c.freqList.Front()
	if bucket == nil {
		return false
	}
	elem := bucket.Value.(*frequencyNode).items.Front()
	if elem.Value == keep {
		if elem.Next() != nil {
			elem = elem.Next()
		} else if bucket.Next() != nil {
			elem = bucket.Next().Value.(*frequencyNode).items.Front()
		} else {
			return false
		}
	}
	item := elem.Value.(*lfuItem)
	c.removeItem(item.key)
	c.stats.evict()
	return true
}

func (c *LFUCache) removeItem(key string) {
	item, exists := c.items[key]
	if !exists {
		return
	}

	node := item.bucket.Value.(*frequencyNode)
	node.items.Remove(item.listElem)
	if node.items.Len() == 0 {
		c.freqList.Remove(item.bucket)
	}
	delete(c.items, key)
	c.currentBytes -= item.cost
}
