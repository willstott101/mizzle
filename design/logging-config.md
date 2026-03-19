# Logging and Configuration

Mizzle is a library.  The consuming application (the "forge") owns the
network edge, the auth backend, the user-facing error surface, and the
operational telemetry pipeline.  Mizzle owns the git protocol machine and
the staging of pack data.  This document draws the line between the two.

## Logging

### Principle

Mizzle uses the `log` crate and emits structured messages at appropriate
levels.  It never configures a logger — that is the forge's job.

What mizzle logs is limited to things only it can observe: protocol-level
events and errors.  Anything that could be a metric, alert trigger, or audit
record should instead be surfaced through a **callback** on `RepoAccess` (or
`SshAuth`), because:

- The forge needs structured data, not parsed log lines.
- Different forges care about different things.
- Log levels and filtering are a deployment concern.

### What mizzle should log

| Level   | What                                                        |
|---------|-------------------------------------------------------------|
| `error` | Protocol errors, malformed packets, I/O failures.           |
| `warn`  | Recoverable oddities (e.g. unknown capability from client). |
| `info`  | Request start/end with repo path and protocol version.      |
| `debug` | Negotiation rounds, ref counts, pack object counts.         |
| `trace` | Raw pkt-line traffic (for development only).                |

### What mizzle should NOT log

- Auth decisions — the forge made them; the forge should log them.
- User identities — mizzle doesn't know them (SSH stores a username
  transiently, but never logs it outside of error paths).
- Pack contents or ref values beyond what's needed for debugging.
- Anything at `info` or above during normal successful operations beyond
  request start/end bookends.

### Current state

Logging is sparse and ad-hoc: mostly `error!` on write failures in spawned
tasks and `info!` on request start.  This is roughly correct in spirit but
lacks request-end logging and structured fields.  No immediate changes
needed — tighten as the API stabilises.

---

## Configuration

### Principle

Mizzle should have very few knobs.  Protocol constants (pkt-line frame size,
sideband chunk limit) are dictated by the git spec and are not configurable.
Internal buffer sizes (piper pipes) are implementation details.

What *is* worth exposing falls into two categories:

1. **Limits** — things that protect the server from abuse.
2. **Identity** — things that identify the server to clients.

### Proposed: `ServerConfig`

A single config struct, shared across transports:

```rust
pub struct ServerConfig {
    /// Included in capability advertisements (`agent=<value>`).
    /// Default: `"mizzle/<version>"`.
    pub agent: String,

    /// Maximum pack size in bytes accepted on push.  `None` = unlimited
    /// (the forge or reverse proxy is responsible for body limits).
    /// Default: `None`.
    pub max_pack_bytes: Option<u64>,
}
```

Anything transport-specific lives on the transport's own config:

- **HTTP**: Body limits, timeouts, TLS — all owned by the framework
  and are not mizzle's concern.
- **SSH**: `russh::server::Config` already covers crypto, keepalives, and
  `inactivity_timeout`.  Mizzle adds only `exec_timeout` (time between
  channel open and exec request).

```rust
pub struct SshConfig {
    pub russh: russh::server::Config,
    /// Time allowed between channel open and exec request.
    /// Default: 10 seconds.
    pub exec_timeout: Duration,
}
```

`run()` / `run_on_socket()` take `SshConfig` instead of raw
`russh::server::Config`.

### What mizzle should NOT configure

- **Connection limits / rate limiting.**  These belong at the network edge
  (load balancer, reverse proxy) or in the forge's middleware.  Mizzle has
  no concept of "too many connections" — it processes one request at a time
  per connection.
- **Repository-level quotas** (max repo size, max refs).  These are policy
  and belong in `RepoAccess` or the forge's storage layer.
- **Logging configuration.**  The forge picks the logger.
