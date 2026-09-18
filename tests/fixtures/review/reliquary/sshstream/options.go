package sshstream

import (
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"fmt"
	"net"
	"strings"
	"time"

	"golang.org/x/crypto/ssh"
	"indecentsoftware.com/reliquary/cache"
)

// Option configures the Manager's default settings
type Option func(*defaultConfig)

// The caller owns the cache because it can be shared by multiple managers.
func WithCache(c cache.Cache) Option {
	return func(cfg *defaultConfig) {
		cfg.cache = c
	}
}

// WithDefaultNetworkChunkSize sets the default network chunk size for new hosts
func WithDefaultNetworkChunkSize(bytes int64) Option {
	return func(c *defaultConfig) {
		c.networkChunkSize = bytes
	}
}

// WithDefaultCacheChunkSize sets the default cache chunk size for new hosts
func WithDefaultCacheChunkSize(bytes int64) Option {
	return func(c *defaultConfig) {
		c.cacheChunkSize = bytes
	}
}

// WithDefaultMaxConnections sets the default maximum connections for new hosts
func WithDefaultMaxConnections(n int) Option {
	return func(c *defaultConfig) {
		c.maxConnections = n
	}
}

// WithDefaultMinConnections sets the default minimum connections for new hosts
func WithDefaultMinConnections(n int) Option {
	return func(c *defaultConfig) {
		c.minConnections = n
	}
}

// WithPrefetch enables or disables prefetching
func WithPrefetch(enabled bool) Option {
	return func(c *defaultConfig) {
		c.prefetchEnabled = enabled
	}
}

// HostConfig contains configuration for connecting to a specific host
type HostConfig struct {
	Hostname           string
	Port               int
	Username           string
	Auth               AuthMethod
	HostKeyFingerprint string

	// Optional host-specific overrides
	Options []HostOption
}

func (c HostConfig) ClientConfig(timeout time.Duration) (*ssh.ClientConfig, error) {
	if c.Auth == nil {
		return nil, fmt.Errorf("SSH authentication is required")
	}
	credentials, err := c.Auth.Resolve()
	if err != nil {
		return nil, fmt.Errorf("configure SSH authentication: %w", err)
	}
	return c.clientConfig(credentials.Methods, timeout)
}

func (c HostConfig) clientConfig(auth []ssh.AuthMethod, timeout time.Duration) (*ssh.ClientConfig, error) {
	callback, err := hostKeyCallback(c.HostKeyFingerprint)
	if err != nil {
		return nil, err
	}
	return &ssh.ClientConfig{
		User:            c.Username,
		Auth:            auth,
		HostKeyCallback: callback,
		Timeout:         timeout,
	}, nil
}

func (c HostConfig) Validate() error {
	if c.Auth == nil {
		return fmt.Errorf("SSH authentication is required")
	}
	_, err := hostKeyCallback(c.HostKeyFingerprint)
	if err != nil {
		return err
	}
	return nil
}

func hostKeyCallback(expected string) (ssh.HostKeyCallback, error) {
	encoded, ok := strings.CutPrefix(expected, "SHA256:")
	if !ok {
		return nil, fmt.Errorf("SSH host key fingerprint must start with SHA256:")
	}
	digest, err := base64.RawStdEncoding.DecodeString(encoded)
	if err != nil || len(digest) != sha256.Size {
		return nil, fmt.Errorf("SSH host key fingerprint is invalid")
	}
	return func(hostname string, _ net.Addr, key ssh.PublicKey) error {
		actual := ssh.FingerprintSHA256(key)
		if subtle.ConstantTimeCompare([]byte(actual), []byte(expected)) == 0 {
			return fmt.Errorf("SSH host key mismatch for %s: expected %s, got %s", hostname, expected, actual)
		}
		return nil
	}, nil
}

// HostOption configures host-specific settings
type HostOption func(*hostConfig)

// WithHostCacheSize sets the cache size for a specific host
func WithHostCacheSize(bytes int64) HostOption {
	return func(c *hostConfig) {
		// Note: This would need to be handled specially since cache is created
		// from the strategy. For now, this is a placeholder.
	}
}

// WithHostNetworkChunkSize sets the network chunk size for a specific host
func WithHostNetworkChunkSize(bytes int64) HostOption {
	return func(c *hostConfig) {
		c.networkChunkSize = bytes
	}
}

// WithHostCacheChunkSize sets the cache chunk size for a specific host
func WithHostCacheChunkSize(bytes int64) HostOption {
	return func(c *hostConfig) {
		c.cacheChunkSize = bytes
	}
}

// WithHostMaxConnections sets the maximum connections for a specific host
func WithHostMaxConnections(n int) HostOption {
	return func(c *hostConfig) {
		c.maxConnections = n
	}
}

// WithHostMinConnections sets the minimum connections for a specific host
func WithHostMinConnections(n int) HostOption {
	return func(c *hostConfig) {
		c.minConnections = n
	}
}

// WithHostTimeout sets the connection timeout for a specific host
func WithHostTimeout(timeout time.Duration) HostOption {
	return func(c *hostConfig) {
		c.timeout = timeout
	}
}

// WithHostPrefetch enables or disables prefetching for a specific host
func WithHostPrefetch(enabled bool) HostOption {
	return func(c *hostConfig) {
		c.prefetchEnabled = enabled
	}
}
