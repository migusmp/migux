# migux

Minimal nginx-like server in Rust with static files + reverse proxy.

## Quick start

```bash
git clone https://github.com/migusmp/migux.git
cd migux
cargo run
```

It tries to load `migux.conf` from the current working directory; if the file is missing or invalid, it falls back to built-in defaults.

## Architecture overview

- `migux` (binary): boots config, tracing, and master process.
- `migux_core`: master/worker, request parsing, location routing.
- `migux_proxy`: reverse proxy with pooling and streaming.
- `migux_static`: static file server with MIME detection.
- `migux_config`: config schema + defaults.

## Request flow

1) Accept TCP connection.
2) Read one HTTP/1.1 request (headers + body).
3) Select server by listen address.
4) Match location by longest prefix.
5) Dispatch to static or proxy handler.

Client keep-alive is supported (multiple requests per connection).
Client requests support `Content-Length` and `Transfer-Encoding: chunked`.

## Configuration (`migux.conf`)

Format is INI-like using sections.

### Minimal working example config

Save this as `migux.conf`:

```ini
[global]
worker_processes = 1
worker_connections = 256
log_level = "info"

[http]
sendfile = true
keepalive_timeout_secs = 65

[server.main]
listen = "0.0.0.0:8080"
server_name = "localhost"
root = "./public"
index = "index.html"

[location.main_root]
server = "main"
path = "/"
type = "static"
```

### Example config (migux.conf style)

```toml
# -------- global --------
[global]
# Number of worker processes.
worker_processes = 1
# Max concurrent connections per worker.
worker_connections = 1024
# Log level: trace|debug|info|warn|error.
log_level = "info"
# Error log output path.
error_log = "/var/log/migux/error.log"

# -------- http --------
[http]
# Enable sendfile (if supported by platform).
sendfile = false
# Idle keep-alive timeout between requests (seconds).
keepalive_timeout_secs = 60
# Access log output path.
access_log = "/var/log/migux/access.log"

# Timeouts (seconds).
client_read_timeout_secs = 10
proxy_connect_timeout_secs = 3
proxy_read_timeout_secs = 30
proxy_write_timeout_secs = 30

# Limits (bytes).
max_request_headers_bytes = 65536
max_request_body_bytes = 10485760
max_upstream_response_headers_bytes = 65536
max_upstream_response_body_bytes = 10485760

# Upstream connection pool.
proxy_pool_max_per_addr = 16
proxy_pool_idle_timeout_secs = 60

# Static cache settings (disk cache for GET on static locations).
cache_dir = "/var/cache/migux"
cache_default_ttl_secs = 30
cache_max_object_bytes = 1048576

# -------- upstreams --------
[upstream.app]
# Single "host:port" or list ["a:1","b:2"].
server = ["127.0.0.1:3000", "127.0.0.1:3001"]
# Load-balancing strategy: "round_robin" or "single".
strategy = "round_robin"

[upstream.app.health]
# Failures before marking the upstream down.
fail_threshold = 2
# Cooldown time before retry (seconds).
cooldown_secs = 10
# Enable active TCP checks.
active = false
# Active check interval (seconds).
interval_secs = 10
# Active check timeout (seconds).
timeout_secs = 1

# -------- servers --------
[server.main]
# HTTP listen address.
listen = "0.0.0.0:8080"
server_name = "localhost"
root = "./public"
index = "index.html"

[server.main.tls]
# HTTPS listen address.
listen = "0.0.0.0:8443"
# Certificate and private key (PEM).
cert_path = "/etc/migux/certs/fullchain.pem"
key_path = "/etc/migux/certs/privkey.pem"
# Redirect HTTP -> HTTPS.
redirect_http = true
# Enable HTTP/2 via ALPN.
http2 = true

# -------- locations --------
[location.main_root]
# Bind this location to server.main.
server = "main"
# Prefix match (longest prefix wins).
path = "/"
# static or proxy.
type = "static"
# Optional override (defaults to server.root/index).
root = "./public"
index = "index.html"

[location.api]
server = "main"
path = "/api"
type = "proxy"
upstream = "app"
# Parsed but not used yet.
strip_prefix = "/api"
# Enable/disable static cache for this location.
cache = false
```

## Proxy behavior

- **Routing**: selects an upstream by round-robin (if configured) and skips down nodes.
- **Prefix strip**: uses `location.path` to strip prefix (nginx-like).
- **Headers**:
  - Removes hop-by-hop headers.
  - Adds `X-Forwarded-For`, `X-Real-IP`, `X-Forwarded-Proto`, `X-Forwarded-Host`.
  - Sets `Connection: keep-alive` to upstream for HTTP/1.1.
- **Keep-alive pool**:
  - Pools connections per concrete upstream address.
  - Each pooled connection stores a read buffer for leftover bytes.
  - Dead sockets are retried with a fresh connection.
- **Streaming request body**:
  - Client bodies are streamed to upstream (no full buffering).
  - Supports Content-Length and chunked requests.
- **Streaming response**:
  - Streams to the client without full buffering.
  - Supports `Transfer-Encoding: chunked` (real chunk parsing + trailers).
  - Supports `Content-Length`.
  - Fallback to EOF-delimited body (non-reusable).
  - Handles no-body responses (1xx, 204, 304, HEAD).

## TLS termination (optional)

If configured, Migux can terminate HTTPS and (optionally) redirect HTTP to HTTPS.

Example:

```ini
[server.main.tls]
listen = "0.0.0.0:8443"
cert_path = "/etc/migux/certs/fullchain.pem"
key_path = "/etc/migux/certs/privkey.pem"
redirect_http = true
http2 = true
```

Test HTTP/2 (requires TLS):

```bash
curl --http2 -k https://localhost:8443/
```

## Static file server

- Resolves files based on `root` and `index`.
- Uses MIME type detection.
- Uses `Connection: keep-alive` when the client allows it (HTTP/1.1).
- Disk-backed cache (optional): stores `.cache` and `.meta` files under `cache_dir` and also keeps a memory copy for hot hits.

## Error responses

Helpers exist for: 404, 405, 408, 413, 431, 500, 502, 501.

## Limitations / TODO

- HTTP/2 is supported only over TLS (ALPN). Cleartext h2c is not supported.
- `strip_prefix` config is parsed but not used yet (uses `location.path`).
- Cache config is wired for static GETs only (proxy/cache not wired yet).
