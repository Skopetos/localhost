# localhost

A from-scratch HTTP/1.1 web server written in Rust, using a single-threaded `epoll`-based event loop. No async runtimes — only `libc` for raw syscalls.

## Features

- HTTP/1.1 compliant (GET, POST, DELETE)
- Non-blocking I/O with `epoll` (single process, single thread)
- Multiple virtual servers on multiple ports simultaneously
- Static file serving with correct MIME types
- Directory listing
- File uploads (multipart/form-data)
- Chunked and unchunked request bodies
- CGI execution (Python, PHP, shell scripts)
- Cookie parsing and in-memory session management
- Request timeouts
- Configurable error pages (400, 403, 404, 405, 413, 500)
- nginx-style configuration file

## Requirements

- Rust (edition 2024)
- Linux (uses `epoll`, Linux-specific syscalls)

## Build

```bash
cargo build --release
```

## Run

```bash
./target/release/localhost default.conf
```

## Configuration

The server reads an nginx-style configuration file. Example:

```nginx
server {
    host 0.0.0.0;
    listen 8080;
    listen 8443;
    server_name localhost mysite.local;
    client_max_body_size 10M;

    error_page 404 ./www/errors/404.html;
    error_page 500 ./www/errors/500.html;

    location / {
        root ./www;
        index index.html;
        allow_methods GET POST DELETE;
        autoindex on;
    }

    location /upload {
        allow_methods POST;
        upload_dir ./www/uploads;
    }

    location /old-page {
        return 301 /new-page;
    }

    location /cgi-bin {
        root ./www/cgi-bin;
        allow_methods GET POST;
        cgi_extension .py;
    }
}

server {
    host 0.0.0.0;
    listen 9090;
    server_name api.local;

    location / {
        root ./api;
        allow_methods GET POST;
    }
}
```

### Directives

| Directive | Scope | Description |
|---|---|---|
| `host` | server | IP address to bind (default: `0.0.0.0`) |
| `listen` | server | Port to listen on (repeatable) |
| `server_name` | server | Virtual host names (space-separated) |
| `client_max_body_size` | server | Max request body (`10M`, `512K`, bytes) |
| `error_page <code> <path>` | server | Custom error page for a status code |
| `root` | location | Filesystem directory to serve from |
| `index` | location | Default file when URL is a directory |
| `allow_methods` | location | Accepted HTTP methods (space-separated) |
| `autoindex` | location | `on` enables directory listing |
| `upload_dir` | location | Directory where uploaded files are saved |
| `cgi_extension` | location | File extension that triggers CGI (e.g. `.py`) |
| `return <code> <url>` | location | HTTP redirect |

**Server selection:** the first server block matching the request's `Host` header on that port is used. If no `server_name` matches, the first server listening on that port is the default.

## Project Structure

```
src/
  main.rs        — entry point, reads config path from argv
  config.rs      — nginx-style config file parser
  server.rs      — TCP socket setup and main event loop driver
  event_loop.rs  — epoll wrapper (add/remove/readable/writable)
  request.rs     — HTTP/1.1 request parser (chunked + unchunked)
  response.rs    — HTTP response builder + default error pages
  router.rs      — longest-prefix route matching + server_name selection
  handler.rs     — GET/POST/DELETE dispatch, static files, uploads, CGI
  cgi.rs         — fork/exec CGI with env vars and stdin/stdout pipes
  session.rs     — in-memory session store with cookie helpers
default.conf     — example configuration file
```

## CGI

Scripts are executed based on file extension configured in the location block. The server sets standard CGI environment variables:

```
REQUEST_METHOD, PATH_INFO, SCRIPT_FILENAME, QUERY_STRING,
CONTENT_LENGTH, CONTENT_TYPE, SERVER_PROTOCOL, GATEWAY_INTERFACE,
HTTP_HOST, HTTP_COOKIE
```

Example Python CGI script (`www/cgi-bin/hello.py`):

```python
#!/usr/bin/env python3
import os

print("Content-Type: text/html")
print()
print(f"<h1>Hello from CGI</h1>")
print(f"<p>Method: {os.environ.get('REQUEST_METHOD')}</p>")
```

## Testing

Run the unit tests:

```bash
cargo test
```

Stress test with siege (≥99.5% availability required):

```bash
siege -b http://127.0.0.1:8080
```

Manual tests to verify:

- `curl http://localhost:8080/` — static file
- `curl http://localhost:8080/missing` — 404 error page
- `curl -X DELETE http://localhost:8080/file.txt` — file deletion
- `curl -F "file=@photo.jpg" http://localhost:8080/upload` — file upload
- `curl http://localhost:8080/cgi-bin/hello.py` — CGI execution
- `curl -H "Transfer-Encoding: chunked" ...` — chunked request body

## Limits

- No TLS/HTTPS support
- CGI scripts are blocking (forked process); very slow CGI will hold that response
- Sessions are in-memory only and lost on restart
