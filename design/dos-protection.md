# DOS Protection

## What is git-specific?

Very little.  Almost all DOS protection is generic HTTP/TCP server hardening
that the forge or its infrastructure handles regardless of mizzle:

- Connection limits, rate limiting → tower middleware / load balancer
- Request body size → hyper / reverse proxy config
- TLS termination → reverse proxy or rustls
- Idle connection reaping → tokio timeouts / russh config

Only two things are git-specific enough to live in mizzle:

1. **Pack size limiting** — mizzle knows it's receiving a packfile and can
   reject before staging to disk with a meaningful error ("pack too large"
   vs a generic "request entity too large").  For SSH there is no framework
   layer, so mizzle must enforce this directly.

2. **Fetch negotiation limits** — only mizzle understands the negotiation
   loop.  A generic request timeout would eventually catch a runaway
   negotiation, but coarsely.  A future `max_negotiation_rounds` on
   `ServerConfig` would cap this precisely.

Everything else belongs in the forge's middleware or example code.

## SSH exec timeout

The SSH server accepts all public keys at the SSH layer and defers real auth
to `SshAuth::authorize` at exec-request time.  This means unauthenticated
clients can hold open SSH sessions.  The `exec_timeout` (default 10s)
actively disconnects clients that open a channel but never send a git
command.  This is enforced by a spawned task, not a passive check.

## Responsibility matrix

| Threat                         | Mitigation                          | Owner                     |
|--------------------------------|-------------------------------------|---------------------------|
| Connection floods              | Connection limits, SYN cookies      | **Forge / infrastructure** |
| Idle SSH sessions              | `exec_timeout`                      | **Mizzle**                |
| Oversized push pack            | `max_pack_bytes`                    | **Mizzle** (backstop)     |
| Infinite fetch negotiation     | `max_negotiation_rounds` (future)   | **Mizzle**                |
| Large clone bandwidth          | Rate-limit at network edge          | **Forge / infrastructure** |
| Rate limiting                  | Tower middleware / load balancer     | **Forge / infrastructure** |
| IP blocking / allowlisting     | Network policy                      | **Forge / infrastructure** |
| Abuse detection                | Domain logic in `RepoAccess`        | **Forge**                 |

## Example code, not library code

Mizzle's example binaries / reference servers should demonstrate sensible
defaults for the forge-owned mitigations: tower concurrency limits, body
size limits, request timeouts, etc.  This makes the "pit of success" easy
to fall into without baking policy into the library.
