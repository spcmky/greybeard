package reliquary

import (
	"context"
	"fmt"
	"sync"

	"golang.org/x/sync/singleflight"
	"indecentsoftware.com/internal/id"
	"indecentsoftware.com/reliquary/cache"
)

type metadataCache[T any] struct {
	mu         sync.RWMutex
	generation uint64
	entries    *cache.LRU[id.UUID, T]
	loads      singleflight.Group
}

func newMetadataCache[T any](capacity int) *metadataCache[T] {
	return &metadataCache[T]{entries: cache.NewLRU[id.UUID, T](capacity)}
}

func (c *metadataCache[T]) get(ctx context.Context, fileID id.UUID, load func(context.Context, id.UUID) (T, error)) (T, error) {
	c.mu.RLock()
	generation := c.generation
	value, ok := c.entries.Get(fileID)
	c.mu.RUnlock()
	if ok {
		return value, nil
	}
	result, err, _ := c.loads.Do(fmt.Sprintf("%s:%d", fileID, generation), func() (any, error) {
		if value, ok := c.entries.Get(fileID); ok {
			return value, nil
		}
		value, err := load(ctx, fileID)
		if err != nil {
			return nil, err
		}
		c.mu.RLock()
		// An invalidated load can finish for its caller, but cannot repopulate the cache.
		if c.generation == generation {
			c.entries.Put(fileID, value)
		}
		c.mu.RUnlock()
		return value, nil
	})
	if err != nil {
		var zero T
		return zero, err
	}
	return result.(T), nil
}

func (c *metadataCache[T]) invalidate(fileID id.UUID) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.generation++
	c.entries.Delete(fileID)
}
