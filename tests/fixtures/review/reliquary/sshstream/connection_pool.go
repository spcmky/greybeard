package sshstream

import (
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"net"
	"os"
	"sync"
	"time"

	"github.com/pkg/sftp"
	"golang.org/x/crypto/ssh"
	"golang.org/x/crypto/ssh/agent"
)

// connectionPool manages SFTP connections to a single host
type connectionPool struct {
	config poolConfig

	mu          sync.Mutex
	connections []*pooledConnection
	available   chan *pooledConnection
	done        chan struct{}
	closed      bool
	creating    int

	// Statistics
	totalCreated int
	totalClosed  int
}

// poolConfig contains configuration for the connection pool
type poolConfig struct {
	hostname           string
	port               int
	username           string
	auth               []ssh.AuthMethod
	hostKeyFingerprint string
	maxConnections     int
	minConnections     int
	timeout            time.Duration
}

// pooledConnection wraps an SFTP client with metadata
type pooledConnection struct {
	ssh      *ssh.Client
	sftp     *sftp.Client
	id       int
	created  time.Time
	lastUsed time.Time
	useCount int64
	pool     *connectionPool
}

// AuthMethod defines authentication methods for SSH
type AuthMethod interface {
	// One snapshot prevents credential changes from crossing pool boundaries.
	Resolve() (Credentials, error)
	Description() string
}

type Credentials struct {
	Identity [32]byte
	Methods  []ssh.AuthMethod
}

// newConnectionPool creates a new connection pool
func newConnectionPool(config poolConfig) (*connectionPool, error) {
	if config.maxConnections <= 0 {
		config.maxConnections = 10
	}
	if config.minConnections <= 0 {
		config.minConnections = 1
	}
	if config.minConnections > config.maxConnections {
		config.minConnections = config.maxConnections
	}
	if config.timeout <= 0 {
		config.timeout = 30 * time.Second
	}

	pool := &connectionPool{
		config:      config,
		connections: make([]*pooledConnection, 0, config.maxConnections),
		available:   make(chan *pooledConnection, config.maxConnections),
		done:        make(chan struct{}),
	}

	// Create minimum connections
	for i := 0; i < config.minConnections; i++ {
		conn, err := pool.createConnection()
		if err != nil {
			// Clean up any created connections
			pool.close()
			return nil, fmt.Errorf("failed to create initial connection %d: %w", i, err)
		}
		pool.connections = append(pool.connections, conn)
		pool.available <- conn
	}

	// Start health checker
	go pool.healthCheck()

	return pool, nil
}

// get retrieves a connection from the pool
func (p *connectionPool) get(ctx context.Context) *pooledConnection {
	for {
		select {
		case conn := <-p.available:
			if p.checkout(conn) {
				return conn
			}
			p.closeConnection(conn)
		case <-p.done:
			return nil
		case <-ctx.Done():
			return nil
		default:
		}

		if conn := p.createConnectionBelow(p.config.maxConnections); conn != nil {
			conn.lastUsed = time.Now()
			conn.useCount++
			return conn
		}

		select {
		case conn := <-p.available:
			if p.checkout(conn) {
				return conn
			}
			p.closeConnection(conn)
		case <-p.done:
			return nil
		case <-ctx.Done():
			return nil
		}
	}
}

func (p *connectionPool) checkout(conn *pooledConnection) bool {
	if conn == nil || !p.isHealthy(conn) {
		return false
	}
	conn.lastUsed = time.Now()
	conn.useCount++
	return true
}

func (p *connectionPool) createConnectionBelow(limit int) *pooledConnection {
	p.mu.Lock()
	if p.closed || len(p.connections)+p.creating >= limit {
		p.mu.Unlock()
		return nil
	}
	p.creating++
	p.mu.Unlock()

	conn, err := p.createConnection()

	p.mu.Lock()
	p.creating--
	if err != nil {
		p.mu.Unlock()
		return nil
	}
	if p.closed {
		p.mu.Unlock()
		p.closeConnection(conn)
		return nil
	}
	p.connections = append(p.connections, conn)
	p.mu.Unlock()
	return conn
}

// put returns a connection to the pool
func (p *connectionPool) put(conn *pooledConnection) {
	if conn == nil {
		return
	}

	p.mu.Lock()
	if p.closed {
		p.mu.Unlock()
		p.closeConnection(conn)
		return
	}
	p.mu.Unlock()

	// Check if connection is still healthy
	if !p.isHealthy(conn) {
		p.closeConnection(conn)
		// Try to maintain minimum connections
		p.maintainMinimum()
		return
	}

	// Return to pool
	select {
	case p.available <- conn:
		// Successfully returned
	default:
		// Pool is full, close the connection
		p.closeConnection(conn)
	}
}

// createConnection creates a new SFTP connection
func (p *connectionPool) createConnection() (*pooledConnection, error) {
	sshConfig, err := (HostConfig{
		Hostname:           p.config.hostname,
		Port:               p.config.port,
		Username:           p.config.username,
		HostKeyFingerprint: p.config.hostKeyFingerprint,
	}).clientConfig(p.config.auth, p.config.timeout)
	if err != nil {
		return nil, err
	}

	// Connect to SSH
	addr := net.JoinHostPort(p.config.hostname, fmt.Sprintf("%d", p.config.port))
	dialer := net.Dialer{Timeout: p.config.timeout}
	netConn, err := dialer.Dial("tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("failed to dial %s: %w", addr, err)
	}

	c, chans, reqs, err := ssh.NewClientConn(netConn, addr, sshConfig)
	if err != nil {
		netConn.Close()
		return nil, fmt.Errorf("failed to establish SSH connection: %w", err)
	}
	sshClient := ssh.NewClient(c, chans, reqs)

	// Create SFTP client
	sftpClient, err := sftp.NewClient(sshClient)
	if err != nil {
		sshClient.Close()
		return nil, fmt.Errorf("failed to create SFTP client: %w", err)
	}

	p.mu.Lock()
	p.totalCreated++
	id := p.totalCreated
	p.mu.Unlock()

	return &pooledConnection{
		ssh:     sshClient,
		sftp:    sftpClient,
		id:      id,
		created: time.Now(),
		pool:    p,
	}, nil
}

// closeConnection closes a connection
func (p *connectionPool) closeConnection(conn *pooledConnection) {
	if conn == nil {
		return
	}

	// Close SFTP first
	if conn.sftp != nil {
		conn.sftp.Close()
	}

	// Then close SSH
	if conn.ssh != nil {
		conn.ssh.Close()
	}

	// Remove from connections list
	p.mu.Lock()
	for i, c := range p.connections {
		if c == conn {
			p.connections = append(p.connections[:i], p.connections[i+1:]...)
			break
		}
	}
	p.totalClosed++
	p.mu.Unlock()
}

// isHealthy checks if a connection is still healthy
func (p *connectionPool) isHealthy(conn *pooledConnection) bool {
	if conn == nil || conn.ssh == nil || conn.sftp == nil {
		return false
	}

	// Try a simple operation to test the connection
	_, err := conn.sftp.Getwd()
	return err == nil
}

// maintainMinimum ensures minimum connections are available
func (p *connectionPool) maintainMinimum() {
	if conn := p.createConnectionBelow(p.config.minConnections); conn != nil {
		p.put(conn)
	}
}

// healthCheck periodically checks connection health
func (p *connectionPool) healthCheck() {
	ticker := time.NewTicker(30 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-ticker.C:
			p.maintainMinimum()
		case <-p.done:
			return
		}
	}
}

// stats returns pool statistics
func (p *connectionPool) stats() poolStats {
	p.mu.Lock()
	defer p.mu.Unlock()

	active := len(p.connections)
	idle := len(p.available)

	return poolStats{
		active:       active,
		idle:         idle,
		totalCreated: p.totalCreated,
		totalClosed:  p.totalClosed,
	}
}

// close closes all connections in the pool
func (p *connectionPool) close() error {
	p.mu.Lock()
	if p.closed {
		p.mu.Unlock()
		return nil
	}
	p.closed = true
	connections := make([]*pooledConnection, len(p.connections))
	copy(connections, p.connections)
	p.connections = nil
	close(p.done)
	p.mu.Unlock()

	// Close all connections
	for _, conn := range connections {
		p.closeConnection(conn)
	}

	return nil
}

// poolStats contains statistics for the connection pool
type poolStats struct {
	active       int
	idle         int
	totalCreated int
	totalClosed  int
}

// Implementation of various AuthMethod types

// sshAgentAuth uses SSH agent for authentication
type sshAgentAuth struct {
	socket string
}

func SSHAgent(socket string) AuthMethod {
	return sshAgentAuth{socket: socket}
}

func (a sshAgentAuth) Resolve() (Credentials, error) {
	if a.socket == "" {
		return Credentials{}, fmt.Errorf("SSH agent socket is not configured")
	}
	conn, err := net.Dial("unix", a.socket)
	if err != nil {
		return Credentials{}, fmt.Errorf("connect to SSH agent: %w", err)
	}
	defer conn.Close()
	keys, err := agent.NewClient(conn).List()
	if err != nil {
		return Credentials{}, fmt.Errorf("list SSH agent keys: %w", err)
	}
	if len(keys) == 0 {
		return Credentials{}, fmt.Errorf("no keys available in SSH agent")
	}
	hash := sha256.New()
	fmt.Fprintf(hash, "agent:%q:", a.socket)
	signers := make([]ssh.Signer, 0, len(keys))
	for _, key := range keys {
		var publicKey ssh.PublicKey
		publicKey, err = ssh.ParsePublicKey(key.Blob)
		if err != nil {
			return Credentials{}, fmt.Errorf("parse SSH agent public key: %w", err)
		}
		fmt.Fprintf(hash, "%x\n", key.Blob)
		signers = append(signers, agentSigner{socket: a.socket, key: publicKey})
	}
	return Credentials{Identity: [32]byte(hash.Sum(nil)), Methods: []ssh.AuthMethod{ssh.PublicKeys(signers...)}}, nil
}

func (a sshAgentAuth) Description() string { return "ssh-agent" }

type agentSigner struct {
	socket string
	key    ssh.PublicKey
}

func (s agentSigner) PublicKey() ssh.PublicKey { return s.key }

func (s agentSigner) Sign(random io.Reader, data []byte) (*ssh.Signature, error) {
	return s.SignWithAlgorithm(random, data, "")
}

func (s agentSigner) SignWithAlgorithm(_ io.Reader, data []byte, algorithm string) (*ssh.Signature, error) {
	conn, err := net.Dial("unix", s.socket)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	key := s.key
	if certificate, ok := key.(*ssh.Certificate); ok {
		key = certificate.Key
	}
	var flags agent.SignatureFlags
	switch algorithm {
	case "", key.Type():
	case ssh.KeyAlgoRSASHA256:
		flags = agent.SignatureFlagRsaSha256
	case ssh.KeyAlgoRSASHA512:
		flags = agent.SignatureFlagRsaSha512
	default:
		return nil, fmt.Errorf("unsupported SSH agent signing algorithm %q", algorithm)
	}
	return agent.NewClient(conn).SignWithFlags(s.key, data, flags)
}

// privateKeyAuth uses a private key file for authentication
type privateKeyAuth struct {
	keyPath    string
	passphrase []byte
}

func PrivateKeyFile(keyPath string) AuthMethod {
	return privateKeyAuth{keyPath: keyPath}
}

func PrivateKeyFileWithPassphrase(keyPath string, passphrase []byte) AuthMethod {
	return privateKeyAuth{keyPath: keyPath, passphrase: passphrase}
}

func (a privateKeyAuth) Resolve() (Credentials, error) {
	signer, err := a.signer()
	if err != nil {
		return Credentials{}, err
	}
	return Credentials{Identity: sha256.Sum256(signer.PublicKey().Marshal()), Methods: []ssh.AuthMethod{ssh.PublicKeys(signer)}}, nil
}

func (a privateKeyAuth) signer() (ssh.Signer, error) {
	key, err := os.ReadFile(a.keyPath)
	if err != nil {
		return nil, fmt.Errorf("failed to read private key: %w", err)
	}

	var signer ssh.Signer
	if len(a.passphrase) > 0 {
		signer, err = ssh.ParsePrivateKeyWithPassphrase(key, a.passphrase)
	} else {
		signer, err = ssh.ParsePrivateKey(key)
	}
	if err != nil {
		return nil, fmt.Errorf("failed to parse private key: %w", err)
	}

	return signer, nil
}

func (a privateKeyAuth) Description() string {
	return fmt.Sprintf("private-key:%s", a.keyPath)
}

// passwordAuth uses password for authentication
type passwordAuth struct {
	password string
}

func Password(password string) AuthMethod {
	return passwordAuth{password: password}
}

func (a passwordAuth) Resolve() (Credentials, error) {
	return Credentials{Identity: sha256.Sum256([]byte("password:" + a.password)), Methods: []ssh.AuthMethod{ssh.Password(a.password)}}, nil
}

func (a passwordAuth) Description() string { return "password" }
