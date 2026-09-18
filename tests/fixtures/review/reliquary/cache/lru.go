package cache

import (
	"container/list"
	"sync"
)

// LRU evicts the least-recently-used entry when full.
type LRU[K comparable, V any] struct {
	mu         sync.Mutex
	maxEntries int
	items      map[K]*list.Element
	order      *list.List
}

type lruEntry[K comparable, V any] struct {
	key K
	val V
}

func NewLRU[K comparable, V any](maxEntries int) *LRU[K, V] {
	return &LRU[K, V]{
		maxEntries: maxEntries,
		items:      make(map[K]*list.Element, maxEntries),
		order:      list.New(),
	}
}

func (c *LRU[K, V]) Get(key K) (V, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()

	elem, ok := c.items[key]
	if !ok {
		var zero V
		return zero, false
	}
	c.order.MoveToFront(elem)
	return elem.Value.(*lruEntry[K, V]).val, true
}

func (c *LRU[K, V]) Put(key K, val V) {
	c.mu.Lock()
	defer c.mu.Unlock()

	if elem, ok := c.items[key]; ok {
		c.order.MoveToFront(elem)
		elem.Value.(*lruEntry[K, V]).val = val
		return
	}

	if c.order.Len() >= c.maxEntries {
		back := c.order.Back()
		c.order.Remove(back)
		delete(c.items, back.Value.(*lruEntry[K, V]).key)
	}

	elem := c.order.PushFront(&lruEntry[K, V]{key: key, val: val})
	c.items[key] = elem
}

func (c *LRU[K, V]) Delete(key K) {
	c.mu.Lock()
	defer c.mu.Unlock()

	elem, ok := c.items[key]
	if !ok {
		return
	}
	c.order.Remove(elem)
	delete(c.items, key)
}

func (c *LRU[K, V]) Clear() {
	c.mu.Lock()
	defer c.mu.Unlock()

	clear(c.items)
	c.order.Init()
}
