# Architecture Notes

This file documents *how* the server works internally — useful for audits,
code review, or anyone extending it. For setup/usage, see `README.md`.

## Single-threaded epoll event loop

Everything — every listener, every client socket, and every CGI pipe — is
registered on one `epoll` instance and driven by one `epoll_wait()` call per
iteration of the loop in `server::run` (`src/server.rs`). There is no thread
pool and no per-connection blocking I/O.

The epoll set is **level-triggered** (no `EPOLLET`). This is a deliberate
choice: it lets the dispatch loop do exactly one `read()`/`write()` syscall
per ready fd per round (`read_client`, `write_client` in `src/server.rs`),
and rely on `epoll_wait()` reporting the fd again on the next round if data
is still buffered. Edge-triggered mode would instead require draining each
fd in an inner loop until `EAGAIN`, which both violates the "one read/write
per client per select" rule and risks one chatty client starving the others
in that same round.

`FdKind` (`src/event_loop.rs`) tags every registered fd so the dispatch loop
knows how to handle it:

- `Listener(port)` — a listening socket; on `EPOLLIN`, `accept_clients` takes
  exactly one connection per event.
- `Client` — a connected client socket, read/written via `read_client`/`write_client`.
- `CgiStdout(client_fd)` / `CgiStdin(client_fd)` — pipe ends of a running CGI
  child, see below.

## Request lifecycle

1. `accept_clients` accepts one connection, sets it non-blocking, and
   registers it with `EPOLLIN`.
2. `read_client` does one `read()`, feeds the bytes to `RequestParser`
   (`src/request.rs`), which understands both `Content-Length` and chunked
   bodies. If the request isn't complete yet, it just waits for the next
   `EPOLLIN`.
3. Once a full request is parsed, `handler::handle` (`src/handler.rs`)
   routes it (`src/router.rs`) and returns a `HandleOutcome`:
   - `Response` — a complete response, ready to serialize and send.
   - `Cgi(CgiProcess)` — a CGI child has been spawned and needs its pipes
     driven asynchronously (see below).
4. For a `Response`, `finish_response` sets the `Connection` header
   (`keep-alive` or `close`) and the client fd is switched to `EPOLLOUT`.
5. `write_client` writes one chunk per ready event; once fully flushed it
   either goes back to `EPOLLIN` (keep-alive) or is closed
   (`Connection: close`, tracked via `Client::close_after_write`).

## CGI: non-blocking by construction

CGI used to run via `fork()` + a blocking `select()` loop on the child's
stdout, called synchronously from inside request handling — this stalled
the *entire* event loop (every other client) for as long as the script ran.

It now works like this (`src/cgi.rs`, `src/server.rs`):

1. `cgi::spawn_cgi` resolves the script to an absolute path (the child
   `chdir()`s into the script's directory for CGI convenience, so the exec
   path must be absolute — a relative path would be looked up relative to
   the *new* cwd and fail), forks, `exec`s, and returns a `CgiProcess` with
   non-blocking stdin/stdout pipe fds. No blocking I/O happens here.
2. `server::start_cgi` registers `stdout_fd` as `CgiStdout(client_fd)` and,
   if there's a request body to forward, `stdin_fd` as `CgiStdin(client_fd)`
   in the same epoll set as every client socket. The originating client fd
   is set idle (`EventLoop::set_idle`) so it stops reporting `EPOLLIN` while
   its CGI response is in flight, but stays registered so a disconnect
   still surfaces via `EPOLLHUP`.
3. `handle_cgi_stdin` writes one chunk of the body per `EPOLLOUT` event;
   `handle_cgi_stdout` reads one chunk of output per `EPOLLIN` event,
   accumulating it until EOF.
4. On stdout EOF, `finalize_cgi` reaps the child (non-blocking `waitpid`,
   falling back to a `reap_queue` swept every tick if the child hasn't
   exited yet), parses the CGI output into headers + body, and hands the
   client back to `finish_response` like any other request.
5. A per-CGI deadline (`CGI_TIMEOUT_SECS`) is checked each tick
   (`timeout_cgi`); a hung script is `SIGKILL`ed and the client gets a 504.

Net effect: a slow CGI script only delays *its own* client. Other
connections keep being served from the same loop in the meantime.

## Configuration validation

`config::validate_config` distinguishes two situations that look similar
but mean opposite things:

- **Genuine conflict** — the same `listen <port>` twice in one server
  block, or two server blocks on the same `host:port` with no
  `server_name` to tell them apart (or an overlapping `server_name`).
  This is rejected at startup with a config error.
- **Virtual hosting** — two server blocks sharing a `host:port` but with
  *different* `server_name`s. This is valid nginx-style behavior: one
  socket is bound, and `router::match_server` picks the right server block
  per request based on the `Host` header.

Separately, `server::run` treats a bind failure on *one* listener as
non-fatal: it logs a warning and skips that listener, but keeps the rest of
the configured servers running. The process only exits if *no* listener
could be bound at all.

## Sessions and cookies

`SessionStore` (`src/session.rs`) is an in-memory `HashMap` keyed by a
generated session id, with a fixed timeout swept periodically. `serve_file`
(`src/handler.rs`) issues a `Set-Cookie: session_id=...` on first contact
and reuses the id from the `Cookie` header on subsequent requests. Sessions
do not survive a server restart.

## Known limitations

- No TLS.
- IPv4 only (`parse_ipv4` in `src/server.rs`).
- Sessions are in-memory only.
- One in-flight CGI request per client connection at a time (fine for
  HTTP/1.1's strictly sequential request/response model, which this server
  follows — no pipelined-response reordering).
