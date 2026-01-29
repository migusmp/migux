# migux

Minimal nginx-like server in Rust with static files + reverse proxy.

## Quick start

```bash
cargo run
```

By default it loads `migux.conf` from the current working directory.

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

Note: only one request per client connection is handled (client keep-alive is not implemented).

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

### [global]

- `worker_processes` (u8)
- `worker_connections` (u16)
- `log_level` (string)
- `error_log` (string path)

### [http]

- `sendfile` (bool)
- `keepalive_timeout_secs` (u64)
- `access_log` (string path)

Timeouts:
- `client_read_timeout_secs`
- `proxy_connect_timeout_secs`
- `proxy_read_timeout_secs`
- `proxy_write_timeout_secs`

Limits (bytes):
- `max_request_headers_bytes`
- `max_request_body_bytes`
- `max_upstream_response_headers_bytes`
- `max_upstream_response_body_bytes`

Cache fields (defined but not wired yet):
- `cache_dir`
- `cache_default_ttl_secs`
- `cache_max_object_bytes`

### [upstream.<name>]

- `server`: single `"host:port"` or list `["a:1","b:2"]`
- `strategy`: `"round_robin"` or `"single"`

### [server.<name>]

- `listen`: `"0.0.0.0:8080"`
- `server_name`
- `root`
- `index`

### [location.<name>]

- `server`: references a `[server.<name>]`
- `path`: prefix match (longest prefix wins)
- `type`: `static` or `proxy`
- `root` (optional, static)
- `index` (optional, static)
- `upstream` (optional, proxy)
- `strip_prefix` (parsed, not used yet)
- `cache` (optional, not used yet)

## Proxy behavior

- **Routing**: selects an upstream by round-robin (if configured).
- **Prefix strip**: uses `location.path` to strip prefix (nginx-like).
- **Headers**:
  - Removes hop-by-hop headers.
  - Adds `X-Forwarded-For`, `X-Real-IP`, `X-Forwarded-Proto`, `X-Forwarded-Host`.
  - Sets `Connection: keep-alive` to upstream for HTTP/1.1.
- **Keep-alive pool**:
  - Pools connections per concrete upstream address.
  - Each pooled connection stores a read buffer for leftover bytes.
  - Dead sockets are retried with a fresh connection.
- **Streaming response**:
  - Streams to the client without full buffering.
  - Supports `Transfer-Encoding: chunked` (real chunk parsing + trailers).
  - Supports `Content-Length`.
  - Fallback to EOF-delimited body (non-reusable).
  - Handles no-body responses (1xx, 204, 304, HEAD).

## Static file server

- Resolves files based on `root` and `index`.
- Uses MIME type detection.
- Always returns `Connection: close`.

## Error responses

Helpers exist for: 404, 405, 408, 413, 431, 500, 502, 501.

## Limitations / TODO

- Client keep-alive not implemented (one request per connection).
- HTTP/2 not supported.
- `strip_prefix` config is parsed but not used yet (uses `location.path`).
- Cache config is not wired yet.
- No TLS.
