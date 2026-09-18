package sshstream

import (
	"crypto/rand"
	"fmt"
	"time"

	"indecentsoftware.com/reliquary/cache"
)

type remoteFile struct {
	path     string
	size     int64
	modified time.Time
}

type chunkCache struct {
	backend   cache.Cache
	namespace string
}

func newChunkCache(backend cache.Cache, config hostConfig) *chunkCache {
	return &chunkCache{
		backend: backend,
		// A new authenticated pool must not inherit bytes fetched under other credentials.
		namespace: fmt.Sprintf("ssh:%s:%q:%d:%q:%q:%d:", rand.Text(), config.hostname, config.port, config.username, config.hostKeyFingerprint, config.networkChunkSize),
	}
}

func (c *chunkCache) key(file remoteFile, offset int64) string {
	return fmt.Sprintf("%s%q:%d:%d:%d", c.namespace, file.path, file.size, file.modified.UnixNano(), offset)
}

func (c *chunkCache) get(file remoteFile, offset int64) ([]byte, bool) {
	return c.backend.Get(c.key(file, offset))
}

func (c *chunkCache) put(file remoteFile, offset int64, data []byte) {
	c.backend.Put(c.key(file, offset), data)
}
