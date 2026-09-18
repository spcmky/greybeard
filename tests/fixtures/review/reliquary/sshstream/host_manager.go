package sshstream

import (
	"context"
	"errors"
	"fmt"
	"io"
	"sync"
	"sync/atomic"
	"time"

	"golang.org/x/crypto/ssh"
	"golang.org/x/sync/singleflight"
)

// hostManager manages streams for a single SSH host (internal, not exported)
type hostManager struct {
	config     hostConfig
	connPool   *connectionPool
	cache      *chunkCache
	fetchGroup singleflight.Group
	analytics  *AnalyticsCollector // Reference to global analytics

	mu      sync.RWMutex
	streams map[*Stream]struct{}
	closed  bool

	// Statistics
	statsData hostStatsData
}

// hostConfig contains configuration for a specific host
type hostConfig struct {
	hostname           string
	port               int
	username           string
	auth               []ssh.AuthMethod
	hostKeyFingerprint string
	networkChunkSize   int64
	cacheChunkSize     int64
	maxConnections     int
	minConnections     int
	prefetchEnabled    bool
	timeout            time.Duration
}

// hostStatsData holds atomic statistics for the host
type hostStatsData struct {
	bytesRead    atomic.Int64
	bytesNetwork atomic.Int64
	cacheHits    atomic.Int64
	cacheMisses  atomic.Int64
	activeReads  atomic.Int32
	totalSeeks   atomic.Int64
}

// newHostManager creates a new host manager
func newHostManager(ctx context.Context, config HostConfig, credentials Credentials, defaults defaultConfig, analytics *AnalyticsCollector) (*hostManager, error) {
	if defaults.cache == nil {
		return nil, fmt.Errorf("cache is required: use WithCache option")
	}

	// Build internal config from HostConfig and defaults
	hc := hostConfig{
		hostname:           config.Hostname,
		port:               config.Port,
		username:           config.Username,
		auth:               credentials.Methods,
		hostKeyFingerprint: config.HostKeyFingerprint,
		networkChunkSize:   defaults.networkChunkSize,
		cacheChunkSize:     defaults.cacheChunkSize,
		maxConnections:     defaults.maxConnections,
		minConnections:     defaults.minConnections,
		prefetchEnabled:    defaults.prefetchEnabled,
		timeout:            30 * time.Second,
	}

	// Apply host-specific options
	for _, opt := range config.Options {
		opt(&hc)
	}

	// Create connection pool
	pool, err := newConnectionPool(poolConfig{
		hostname:           hc.hostname,
		port:               hc.port,
		username:           hc.username,
		auth:               hc.auth,
		hostKeyFingerprint: hc.hostKeyFingerprint,
		maxConnections:     hc.maxConnections,
		minConnections:     hc.minConnections,
		timeout:            hc.timeout,
	})
	if err != nil {
		return nil, fmt.Errorf("failed to create connection pool: %w", err)
	}

	hm := &hostManager{
		config:    hc,
		connPool:  pool,
		cache:     newChunkCache(defaults.cache, hc),
		analytics: analytics,
		streams:   make(map[*Stream]struct{}),
	}

	return hm, nil
}

// open creates a new stream for the given path
func (hm *hostManager) open(ctx context.Context, path string) (*Stream, error) {
	hm.mu.RLock()
	if hm.closed {
		hm.mu.RUnlock()
		return nil, fmt.Errorf("host manager is closed")
	}
	hm.mu.RUnlock()

	stream, err := hm.createStream(ctx, path)
	if err != nil {
		return nil, err
	}

	hm.mu.Lock()
	if hm.closed {
		hm.mu.Unlock()
		_ = stream.Close()
		return nil, fmt.Errorf("host manager is closed")
	}
	hm.streams[stream] = struct{}{}
	hm.mu.Unlock()
	return stream, nil
}

// createStream creates a new stream instance
func (hm *hostManager) createStream(ctx context.Context, path string) (*Stream, error) {
	// Get file info to determine size
	conn := hm.connPool.get(ctx)
	if conn == nil {
		return nil, fmt.Errorf("no available connections")
	}
	defer hm.connPool.put(conn)

	file, err := conn.sftp.Open(path)
	if err != nil {
		return nil, fmt.Errorf("failed to open remote file %s: %w", path, err)
	}
	defer file.Close()

	info, err := file.Stat()
	if err != nil {
		return nil, fmt.Errorf("failed to stat remote file %s: %w", path, err)
	}
	remote := remoteFile{path: path, size: info.Size(), modified: info.ModTime()}

	// Create the chunk fetcher adapter
	fetcher := &hostManagerFetcher{
		manager: hm,
		file:    remote,
	}

	// Create the chunk-aligned reader
	reader := NewChunkAlignedReader(fetcher, path, info.Size())

	// Generate session ID
	sessionID := fmt.Sprintf("%s-%s-%d", hm.config.hostname, path, time.Now().UnixNano())

	stream := &Stream{
		manager:   hm,
		path:      path,
		file:      remote,
		size:      info.Size(),
		reader:    reader,
		sessionID: sessionID,
		collector: hm.analytics,
	}

	// Start analytics session if collector is available
	if hm.analytics != nil {
		hostKey := fmt.Sprintf("%s:%d", hm.config.hostname, hm.config.port)
		stream.analytics = hm.analytics.StartSession(sessionID, hostKey, path, info.Size())
	}

	// Start prefetcher if enabled
	if hm.config.prefetchEnabled {
		stream.prefetcher = newPrefetcher(stream, hm)
		go stream.prefetcher.run()
	}

	return stream, nil
}

// fetchChunk fetches a network-aligned chunk with singleflight deduplication
func (hm *hostManager) fetchChunk(file remoteFile, alignedOffset int64) ([]byte, error) {
	key := hm.cache.key(file, alignedOffset)

	// Check LFU cache first
	if cached, ok := hm.cache.get(file, alignedOffset); ok {
		hm.statsData.cacheHits.Add(1)
		return cached, nil
	}

	hm.statsData.cacheMisses.Add(1)

	// Use singleflight to deduplicate concurrent fetches
	result, err, shared := hm.fetchGroup.Do(key, func() (interface{}, error) {
		// Double-check cache inside singleflight
		// (another request might have completed while we waited)
		if cached, ok := hm.cache.get(file, alignedOffset); ok {
			return cached, nil
		}

		// Fetch from network
		return hm.fetchFromNetwork(file, alignedOffset)
	})

	if err != nil {
		return nil, err
	}

	data := result.([]byte)

	// Only count network bytes if we actually fetched (not shared)
	if !shared {
		hm.statsData.bytesNetwork.Add(int64(len(data)))
	}

	// Store in cache (LFU cache will track frequency)
	hm.cache.put(file, alignedOffset, data)

	return data, nil
}

// fetchFromNetwork performs the actual network fetch
func (hm *hostManager) fetchFromNetwork(remote remoteFile, alignedOffset int64) ([]byte, error) {
	ctx, cancel := context.WithTimeoutCause(context.Background(), hm.config.timeout,
		fmt.Errorf("sftp fetch timed out for %s at offset %d", remote.path, alignedOffset))
	defer cancel()

	// Get connection from pool
	conn := hm.connPool.get(ctx)
	if conn == nil {
		return nil, fmt.Errorf("no available connections")
	}
	defer hm.connPool.put(conn)

	// Open file on SFTP
	file, err := conn.sftp.Open(remote.path)
	if err != nil {
		return nil, fmt.Errorf("failed to open remote file: %w", err)
	}
	defer file.Close()

	// Seek to aligned position
	if _, err := file.Seek(alignedOffset, io.SeekStart); err != nil {
		return nil, fmt.Errorf("failed to seek to %d: %w", alignedOffset, err)
	}

	// Get file size to handle EOF properly
	info, err := file.Stat()
	if err != nil {
		return nil, fmt.Errorf("failed to stat file: %w", err)
	}
	if info.Size() != remote.size || !info.ModTime().Equal(remote.modified) {
		return nil, fmt.Errorf("remote file changed during streaming: %s", remote.path)
	}

	// Calculate chunk size (may be less than full chunk at EOF)
	chunkSize := hm.config.networkChunkSize
	if alignedOffset+chunkSize > info.Size() {
		chunkSize = info.Size() - alignedOffset
	}

	if chunkSize <= 0 {
		return []byte{}, io.EOF
	}

	// Track active reads
	hm.statsData.activeReads.Add(1)
	defer hm.statsData.activeReads.Add(-1)

	// Read the chunk
	buffer := make([]byte, chunkSize)
	n, err := io.ReadFull(file, buffer)
	if err != nil && err != io.ErrUnexpectedEOF && err != io.EOF {
		return nil, fmt.Errorf("failed to read chunk at offset %d: %w", alignedOffset, err)
	}
	info, err = file.Stat()
	if err != nil {
		return nil, fmt.Errorf("failed to stat file after read: %w", err)
	}
	if info.Size() != remote.size || !info.ModTime().Equal(remote.modified) {
		return nil, fmt.Errorf("remote file changed during streaming: %s", remote.path)
	}

	return buffer[:n], nil
}

// stats returns current statistics for the host
func (hm *hostManager) stats() HostStats {
	hm.mu.RLock()
	streamCount := len(hm.streams)
	hm.mu.RUnlock()

	cacheHits := hm.statsData.cacheHits.Load()
	cacheMisses := hm.statsData.cacheMisses.Load()

	var hitRate float64
	if total := cacheHits + cacheMisses; total > 0 {
		hitRate = float64(cacheHits) / float64(total)
	}

	poolStats := hm.connPool.stats()

	return HostStats{
		BytesRead:         hm.statsData.bytesRead.Load(),
		BytesNetwork:      hm.statsData.bytesNetwork.Load(),
		CacheHits:         cacheHits,
		CacheMisses:       cacheMisses,
		CacheHitRate:      hitRate,
		ActiveReads:       int(hm.statsData.activeReads.Load()),
		TotalSeeks:        hm.statsData.totalSeeks.Load(),
		StreamCount:       streamCount,
		ConnectionsActive: poolStats.active,
		ConnectionsIdle:   poolStats.idle,
		CacheStats:        hm.cache.backend.Stats(),
	}
}

// close closes the host manager and releases all resources
func (hm *hostManager) close() error {
	hm.mu.Lock()
	if hm.closed {
		hm.mu.Unlock()
		return nil
	}
	hm.closed = true
	streams := make([]*Stream, 0, len(hm.streams))
	for stream := range hm.streams {
		streams = append(streams, stream)
	}
	hm.streams = make(map[*Stream]struct{})
	hm.mu.Unlock()

	var errs []error
	for _, stream := range streams {
		if err := stream.Close(); err != nil {
			errs = append(errs, fmt.Errorf("close stream %s: %w", stream.path, err))
		}
	}

	if err := hm.connPool.close(); err != nil {
		errs = append(errs, fmt.Errorf("close connection pool: %w", err))
	}

	return errors.Join(errs...)
}
