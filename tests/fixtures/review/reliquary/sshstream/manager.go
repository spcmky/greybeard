package sshstream

import (
	"context"
	"errors"
	"fmt"
	"net"
	"strconv"
	"sync"
	"time"

	"indecentsoftware.com/reliquary/cache"
)

// Manager manages SSH streams across multiple hosts.
// It internally maintains a pool of host-specific managers
// but presents a unified interface to callers.
type Manager struct {
	mu        sync.RWMutex
	hosts     map[connectionIdentity]*hostManager
	defaults  defaultConfig       // Default settings for new hosts
	analytics *AnalyticsCollector // Analytics aggregator
	closed    bool
}

type connectionIdentity struct {
	hostname    string
	port        int
	username    string
	fingerprint string
	credentials [32]byte
}

func (i connectionIdentity) address() string {
	return net.JoinHostPort(i.hostname, strconv.Itoa(i.port))
}

func (i connectionIdentity) String() string {
	return fmt.Sprintf("%q@%s", i.username, i.address())
}

// defaultConfig holds default configuration for new hosts
type defaultConfig struct {
	networkChunkSize int64
	cacheChunkSize   int64
	cache            cache.Cache
	maxConnections   int
	minConnections   int
	prefetchEnabled  bool
}

// New creates a new Manager with the provided options.
// A cache.Cache must be provided via WithCache option.
func New(opts ...Option) *Manager {
	m := &Manager{
		hosts: make(map[connectionIdentity]*hostManager),
		defaults: defaultConfig{
			networkChunkSize: 64 * 1024 * 1024, // 64MB network chunks
			cacheChunkSize:   1 * 1024 * 1024,  // 1MB cache chunks
			maxConnections:   10,
			minConnections:   2,
			prefetchEnabled:  true,
		},
		// Initialize analytics with reasonable defaults
		analytics: NewAnalyticsCollector(1000, 24*time.Hour), // Keep 1000 sessions or 24 hours
	}

	// Apply options
	for _, opt := range opts {
		opt(&m.defaults)
	}

	return m
}

// Open opens a stream to a remote file
func (m *Manager) Open(ctx context.Context, config HostConfig, path string) (*Stream, error) {
	if err := config.Validate(); err != nil {
		return nil, err
	}
	// Get or create host manager
	hm, err := m.getOrCreateHost(ctx, config)
	if err != nil {
		return nil, fmt.Errorf("failed to get host manager: %w", err)
	}

	// Open stream through host manager
	return hm.open(ctx, path)
}

// getOrCreateHost returns an existing host manager or creates a new one
func (m *Manager) getOrCreateHost(ctx context.Context, config HostConfig) (*hostManager, error) {
	if config.Port == 0 {
		config.Port = 22
	}
	credentials, err := config.Auth.Resolve()
	if err != nil {
		return nil, fmt.Errorf("identify SSH credentials: %w", err)
	}
	key := connectionIdentity{hostname: config.Hostname, port: config.Port, username: config.Username, fingerprint: config.HostKeyFingerprint, credentials: credentials.Identity}

	// Fast path: check with read lock
	m.mu.RLock()
	if m.closed {
		m.mu.RUnlock()
		return nil, fmt.Errorf("manager is closed")
	}
	if hm, exists := m.hosts[key]; exists {
		m.mu.RUnlock()
		return hm, nil
	}
	m.mu.RUnlock()

	// Slow path: create with write lock
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return nil, fmt.Errorf("manager is closed")
	}

	// Double-check after acquiring write lock
	if hm, exists := m.hosts[key]; exists {
		return hm, nil
	}

	// Create new host manager with analytics
	hm, err := newHostManager(ctx, config, credentials, m.defaults, m.analytics)
	if err != nil {
		return nil, fmt.Errorf("failed to create host manager for %s: %w", key, err)
	}

	m.hosts[key] = hm
	return hm, nil
}

// Stats returns aggregated statistics for all hosts
func (m *Manager) Stats() ManagerStats {
	m.mu.RLock()
	defer m.mu.RUnlock()

	stats := ManagerStats{
		Hosts: make(map[string]HostStats),
	}

	for identity, hm := range m.hosts {
		current := hm.stats()
		key := identity.address()
		host := stats.Hosts[key]
		host.Hostname = key
		host.BytesRead += current.BytesRead
		host.BytesNetwork += current.BytesNetwork
		host.CacheHits += current.CacheHits
		host.CacheMisses += current.CacheMisses
		host.ActiveReads += current.ActiveReads
		host.TotalSeeks += current.TotalSeeks
		host.StreamCount += current.StreamCount
		host.ConnectionsActive += current.ConnectionsActive
		host.ConnectionsIdle += current.ConnectionsIdle
		host.CacheStats = current.CacheStats
		if total := host.CacheHits + host.CacheMisses; total > 0 {
			host.CacheHitRate = float64(host.CacheHits) / float64(total)
		}
		stats.Hosts[key] = host
		stats.TotalBytesRead += current.BytesRead
		stats.TotalBytesNetwork += current.BytesNetwork
		stats.TotalCacheHits += current.CacheHits
		stats.TotalCacheMisses += current.CacheMisses
	}

	if total := stats.TotalCacheHits + stats.TotalCacheMisses; total > 0 {
		stats.CacheHitRate = float64(stats.TotalCacheHits) / float64(total)
	}

	return stats
}

// Analytics returns the analytics collector
func (m *Manager) Analytics() *AnalyticsCollector {
	return m.analytics
}

// Close closes all host managers and releases resources
func (m *Manager) Close() error {
	m.mu.Lock()
	if m.closed {
		m.mu.Unlock()
		return nil
	}
	m.closed = true
	hosts := m.hosts
	m.hosts = make(map[connectionIdentity]*hostManager)
	m.mu.Unlock()

	var errs []error
	for host, hm := range hosts {
		if err := hm.close(); err != nil {
			errs = append(errs, fmt.Errorf("failed to close host %s: %w", host, err))
		}
	}

	m.analytics.Close()

	if len(errs) > 0 {
		return fmt.Errorf("errors closing hosts: %v", errs)
	}
	return nil
}

// CloseHost closes a specific host's manager
func (m *Manager) CloseHost(hostname string, port int) error {
	if port == 0 {
		port = 22
	}

	m.mu.Lock()
	var hosts []*hostManager
	for identity, hm := range m.hosts {
		if identity.hostname == hostname && identity.port == port {
			hosts = append(hosts, hm)
			delete(m.hosts, identity)
		}
	}
	m.mu.Unlock()

	var errs []error
	for _, hm := range hosts {
		if err := hm.close(); err != nil {
			errs = append(errs, fmt.Errorf("close host %s: %w", hostname, err))
		}
	}
	return errors.Join(errs...)
}
