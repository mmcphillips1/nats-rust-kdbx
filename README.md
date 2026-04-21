# nats-rust-kdbx

A Rust shared library (`cdylib`) that embeds inside kdb+ via `2:` dynamic loading, providing native NATS JetStream and core NATS publish/subscribe for q processes. Serialisation uses kdb+ IPC binary — no JSON, no type loss.

## Architecture

```
┌─────────────────────────────┐    ┌──────────────────┐    ┌─────────────────────────────┐
│   q publisher process       │    │   NATS Server    │    │   q subscriber process      │
│                             │    │   (JetStream)    │    │                             │
│   nats_connect[host; port]  │    │                  │    │   nats_connect[host; port]  │
│   nats_publish[subj; table] │    │   :4222          │    │   nats_subscribe[stream;    │
│                             │    │                  │    │     subj; name; policy;     │
│   ┌─────────────────────┐   │    │  ┌────────────┐  │    │     `callback]              │
│   │  libkdb_plugin.so   │───────>│  │  Stream /  │  │    │   ┌─────────────────────┐   │
│   │  (Rust cdylib)      │   │    │  │  Consumer  │──────>│   │  libkdb_plugin.so   │   │
│   └─────────────────────┘   │    │  └────────────┘  │    │   │  (Rust cdylib)      │   │
│                             │    │                  │    │   └──────────┬──────────┘   │
│    kdb+ IPC binary encode   │    │   persistence    │    │              │ sd1 pip      │
│                             │    │   + replay       │    │   callback[decoded_K]       │
└─────────────────────────────┘    └──────────────────┘    └─────────────────────────────┘
```

Rust is loaded into q via `2:` (`dlopen`). There is no separate Rust process. The subscriber uses a background OS thread with a Unix pipe and q's `sd1` event-loop integration to deliver decoded messages back to q's main thread.

### Publisher thread model

All publish calls run on q's main thread. The shared tokio runtime executes async NATS I/O via `block_on`.

```
q main thread (single-threaded)
│
│  nats_publish[subj; data]
│  ├── q_ipc_encode(3)            encode K → IPC bytes
│  └── rt().block_on(...)         blocks until NATS I/O completes
│       │
│       ▼
│  ┌─────────────────────────┐
│  │  Shared tokio runtime   │   Runtime::new() defaults to 1 worker
│  │  (multi-thread, lazy)   │   thread per CPU core. Only 1 is needed
│  │                         │   here — block_on runs the future on the
│  │  js.publish().await     │   calling thread; worker threads handle
│  │  ack_future.await       │   background I/O polling only. Consider
│  └─────────────────────────┘   worker_threads(1) or current_thread to
|                                avoid spawning unnecessary OS threads.
│
│  nats_publish_async[subj; data]
│  ├── q_ipc_encode(3)            encode K → IPC bytes
│  ├── rt().block_on(publish)     send only — returns immediately
│  └── ack future → PENDING_ACKS  queued, not awaited
│
│  nats_flush[(::)]
│  └── rt().block_on(join_all)    await all queued acks in parallel
│
│  nats_publish_core[subj; data]
│  ├── q_ipc_encode(3)            encode K → IPC bytes
│  └── rt().block_on(publish)     fire-and-forget, no ack
```

### Subscriber thread model

`nats_subscribe` spawns a dedicated OS thread with its own `current_thread` tokio runtime. Decoded messages are delivered to q's main thread via a Unix pipe and `sd1`. The NATS message is **acked only after the q callback succeeds** (at-least-once delivery). On callback failure, the ack is skipped and the message is redelivered after the server's `ack_wait` timeout.

```
q main thread                          Background consumer thread
─────────────                          ──────────────────────────
nats_subscribe[...]                    std::thread::spawn(...)
├── pipe()  → (read_fd, write_fd)      │
├── register_callback(read_fd, handler)│  Dedicated current_thread tokio runtime
│                                      │  (single OS thread — K ptrs are safe)
│                                      │
│                                      │  loop:
│   ┌──────────────────────────┐       │    msg = messages.next().await
│   │  q event loop (sd1)      │       │    pin_symbol()
│   │                          │       │    q_ipc_decode(bytes) → K
│   │  nats_msg_handler(fd)    │ <───  │    unpin_symbol()
│   │  ├── read(fd) → ptr      │ pipe  │    Box(PendingMsg{K, msg})
│   │  ├── k(0, callback, K)   │       │    write(pipe, &ptr, 8)
│   │  │   callback[decoded_K] │       │
│   │  ├── on success:         │       │
│   │  │   msg.ack().await     │       │  (ack deferred until callback succeeds)
│   │  └── on error:           │       │
│   │      skip ack → redeliver│       │
│   └──────────────────────────┘       │
```

Two separate tokio runtimes ensure publisher `block_on` calls and the subscriber's infinite loop never contend with each other.

### Rust-side IPC encode/decode

Serialisation is performed entirely in Rust via the kxkdb C API (`q_ipc_encode` / `q_ipc_decode`), not in q with `-8!` / `-9!`. This is a deliberate design choice validated by benchmarking.

The alternative — encoding in q with `-8!` before the FFI call, and decoding in q with `-9!` after receiving raw bytes — was tested across 12 benchmark configurations. It produced an average regression of **+25% p50**, **+19% p99**, and **+18% mean latency**, with the worst config showing +143% p99.

The root cause is **lost pipelining**. With Rust-side decode on the consumer thread, decode of message N+1 runs in parallel with q's callback processing of message N:

```
Rust decodes (current — pipelined):
  Consumer thread:  [decode N] [decode N+1] [decode N+2] ...
  Main thread:      [callback N] [callback N+1] [callback N+2] ...
                    ↑ overlapped — decode and callback run in parallel

q decodes (rejected — serialised):
  Consumer thread:  [wrap N] [wrap N+1] ...  ← almost instant, idles
  Main thread:      [decode+callback N] [decode+callback N+1] ...
                    ↑ serial — no overlap
```

The Rust approach requires `pin_symbol()` / `unpin_symbol()` to safely intern kdb+ symbols from the background thread, but the ~20% latency improvement justifies the added complexity.

### Subscriber: dedicated tokio runtime vs spawn_blocking

The subscriber's consumer thread creates its own `current_thread` tokio runtime rather than using `spawn_blocking` on the shared publisher runtime. Three reasons:

1. **No nested `block_on`** — the publisher APIs (`nats_publish`, etc.) call `rt().block_on()` from q's main thread. If the subscriber shared this runtime via `spawn_blocking`, any async work inside that closure would need its own `block_on`, which panics inside an existing tokio context. A separate `current_thread` runtime avoids this entirely.

2. **K pointer safety** — raw K pointers from `q_ipc_decode` are not `Send`. A `current_thread` runtime guarantees the future runs on a single OS thread with no task migration across `.await` points. A multi-threaded runtime could move the task to a different worker thread after an `.await`, invalidating the raw pointer.

3. **Isolation** — the subscriber loop runs indefinitely. On a shared runtime, a blocked or slow consumer could starve the publisher's short-lived `block_on` calls. A dedicated runtime keeps publisher and subscriber workloads independent.

---

## Build

### Prerequisites

- Rust toolchain (1.70+)
- `pkg-config` and `libssl-dev` (Ubuntu/Debian)
- kdb+ (q binary) for running the plugin

### Local build

```bash
sudo apt-get install -y pkg-config libssl-dev

cargo build --lib --release
# produces: target/release/libkdb_plugin.so
```

### Docker build

```bash
docker build -f Dockerfile.rust -t nats-kdb-rust .
```

The `Dockerfile.rust` is a multi-stage build:
1. **Builder** (`rust:1.88-slim`): compiles the cdylib
2. **Runtime** (`ubuntu:22.04`): copies just `libkdb_plugin.so`

Mount your q installation at runtime:
```yaml
volumes:
  - ${QHOME}:/q:ro
```

---

## Quick Start

```bash
# Start NATS JetStream
cd examples
mkdir -p nats-data
docker compose up -d

# Terminal 1 — subscriber (start first)
q examples/subscriber.q -p 5010

# Terminal 2 — publisher
q examples/publisher_sync.q
```

---

## Loading the Plugin in q

```q
PLUGIN: `./target/release/libkdb_plugin    / path to .so (no extension needed)

nats_connect:        PLUGIN 2: (`nats_connect;        2)
jetstream_init:      PLUGIN 2: (`jetstream_init;      2)
nats_publish:        PLUGIN 2: (`nats_publish;        2)
nats_publish_core:   PLUGIN 2: (`nats_publish_core;   2)
nats_publish_noack:  PLUGIN 2: (`nats_publish_noack;  2)
nats_publish_async:  PLUGIN 2: (`nats_publish_async;  2)
nats_set_flush_size: PLUGIN 2: (`nats_set_flush_size; 1)
nats_flush:          PLUGIN 2: (`nats_flush;           1)
nats_subscribe:      PLUGIN 2: (`nats_subscribe;      5)
```

`2:` calls `dlopen` on the path, resolves each symbol by name, and wraps it as a q function. The Rust function must be `#[no_mangle] extern "C"` and compiled as `crate-type = ["cdylib"]`.

---

## API Reference

### `nats_connect[host; port]` — required

| Arg | Type | Description |
|-----|------|-------------|
| `host` | symbol | NATS server hostname, e.g. `` `localhost `` |
| `port` | int | NATS server port, e.g. `4222i` |

Establishes the NATS TCP connection and caches it for the process lifetime. All other APIs share this connection. Subsequent calls are silently ignored.

**Must be called before any other API.**

```q
nats_connect[`localhost; 4222i]
nats_connect[`my-nats-server; 4222i]
```

---

### `jetstream_init[stream_name; subject]` — optional

| Arg | Type | Description |
|-----|------|-------------|
| `stream_name` | symbol | JetStream stream name, e.g. `` `TRADES `` |
| `subject` | symbol | Subject pattern, e.g. `` `trades.tick `` |

Creates the JetStream stream if it does not already exist. Uses `get_or_create_stream` — fully idempotent. Skip this call if the stream is pre-configured on the server.

```q
jetstream_init[`TRADES; `trades.tick]
```

---

### `nats_publish[subject; payload]` — JetStream sync ack

| Arg | Type | Description |
|-----|------|-------------|
| `subject` | symbol | NATS subject, e.g. `` `trades.tick `` |
| `payload` | K | Any kdb+ object (table, dict, list, atom) |

Encodes the K object as kdb+ IPC binary via `q_ipc_encode(3)`, publishes to JetStream, and **blocks until the server acknowledges** persistence. One round-trip per message.

```q
nats_publish[`trades.tick; trades]         / table
nats_publish[`trades.tick; last trades]    / dict (single row)
nats_publish[`trades.tick; 42j]            / atom
```

**Requires:** `nats_connect`, `jetstream_init`

---

### `nats_publish_core[subject; payload]` — core NATS fire-and-forget

| Arg | Type | Description |
|-----|------|-------------|
| `subject` | symbol | NATS subject |
| `payload` | K | Any kdb+ object |

Publishes via core NATS (no JetStream). Fire-and-forget — returns immediately, no persistence guarantee. If a JetStream stream is configured to capture the subject, messages are still persisted server-side.

```q
nats_publish_core[`trades.tick; trades]
```

**Requires:** `nats_connect`

---

### `nats_publish_noack[subject; payload]` — JetStream no-ack

| Arg | Type | Description |
|-----|------|-------------|
| `subject` | symbol | NATS subject |
| `payload` | K | Any kdb+ object |

Publishes to JetStream but **drops the ack future** immediately. The message is persisted by the server, but the publisher does not wait for confirmation.

```q
nats_publish_noack[`trades.tick; trades]
```

**Requires:** `nats_connect`, `jetstream_init`

---

### `nats_publish_async[subject; payload]` — JetStream pipelined

| Arg | Type | Description |
|-----|------|-------------|
| `subject` | symbol | NATS subject |
| `payload` | K | Any kdb+ object |

Publishes to JetStream and **queues the ack future** for later collection. Returns immediately — no per-message blocking. Call `nats_flush` after the publish loop to await all queued acks in parallel.

If `nats_set_flush_size` was called with N > 0, auto-flushes every N queued messages.

```q
do[1000; nats_publish_async[`trades.tick; genTrade 10]];
nats_flush[(::)]
```

**Requires:** `nats_connect`, `jetstream_init`

---

### `nats_set_flush_size[n]` — auto-flush threshold

| Arg | Type | Description |
|-----|------|-------------|
| `n` | int/long | Flush every N queued acks. 0 = disabled (default). |

When N > 0, `nats_publish_async` automatically flushes after every N queued ack futures. Set to 0 to disable auto-flush and rely on manual `nats_flush` calls.

```q
nats_set_flush_size[1000]   / auto-flush every 1000 messages
nats_set_flush_size[0]      / disable auto-flush
```

---

### `nats_flush[(::)]` — drain pipelined acks

| Arg | Type | Description |
|-----|------|-------------|
| `(::)` | general null | Dummy argument (q requires at least 1 arg) |

Awaits all ack futures queued by `nats_publish_async` in parallel. Blocks until every ack is confirmed (or fails). Prints a summary.

**Must be called** after a `nats_publish_async` loop before process exit.

```q
nats_flush[(::)]
/ [kdb-plugin] nats_flush | 1000 acks confirmed
```

---

### `nats_subscribe[stream; subject; consumer; deliver_policy; callback]`

| Arg | Type | Description |
|-----|------|-------------|
| `stream` | symbol | JetStream stream name, e.g. `` `TRADES `` |
| `subject` | symbol | Subject filter, e.g. `` `trades.tick `` |
| `consumer` | symbol | Durable consumer name (server-side), e.g. `` `my_consumer `` |
| `deliver_policy` | symbol | `` `all ``, `` `last ``, `` `new ``, or `` `last_per_subject `` |
| `callback` | symbol | q function name called per message, e.g. `` `on_trade `` |

Attaches a durable pull consumer to the stream and spawns a background OS thread. **Returns immediately** — non-blocking for q.

The callback receives the exact K type that was published (table, dict, atom, etc.). No manual deserialization needed. Messages use **at-least-once delivery** — the NATS ack is sent only after the callback succeeds. If the callback returns an error, the ack is skipped and the message is redelivered after the server's `ack_wait` timeout.

#### Deliver policies

| Policy | Behaviour |
|--------|-----------|
| `` `all `` | Replay entire stream from the beginning |
| `` `last `` | Start from the last message only |
| `` `new `` | Only receive messages published after subscribing |
| `` `last_per_subject `` | One message per unique subject (latest) |

```q
on_trade:{[t]
    $[98h = type t;
        `trades upsert t;
      99h = type t;
        `trades upsert enlist t;
      -1 "Received: ",.Q.s t
    ];
    }

nats_subscribe[`TRADES; `trades.tick; `my_consumer; `all; `on_trade]
```

**Requires:** `nats_connect`, `jetstream_init` (or stream pre-configured on server)

---

## Serialisation

| Direction | Method | Detail |
|-----------|--------|--------|
| q → NATS | `q_ipc_encode(3)` | Encodes any K object as IPC binary bytes |
| NATS → q | `q_ipc_decode()` | Decodes IPC bytes back to original K type |

The subscriber always receives the **exact same K type** that was published. No type information is lost. This is the same binary format used by kdb+ IPC (`-8!` / `-9!`) but handled entirely in Rust via the kxkdb C API bindings.

---

## Subscriber Callback Mechanism

The subscriber uses q's `sd1` event-loop registration (the "plumber pattern") to safely deliver messages from a background thread to q's main thread. The NATS message handle is bundled with the decoded K value in a `PendingMsg` struct so the ack can be deferred until the callback succeeds:

```
Background consumer thread              q main thread
────────────────────────────             ─────────────
NATS message arrives
  pin_symbol()                       ←── register_callback(read_fd, handler)
  q_ipc_decode(bytes) → K                watches read_fd via q event loop
  unpin_symbol()
  Box(PendingMsg{decoded_K, msg})
  write(pipe_write_fd, &ptr, 8)  ──────► nats_msg_handler(read_fd)
                                          read(read_fd, &ptr, 8)
                                          Box::from_raw(ptr) → PendingMsg
                                          k(0, "callback", decoded_K, KNULL)
                                          callback succeeds → msg.ack()
                                          callback fails    → skip ack (redeliver)
```

`pin_symbol()` / `unpin_symbol()` lock q's symbol intern table so symbols can be safely created from the background thread. The deferred ack pattern provides **at-least-once delivery** — if the q callback errors or the process crashes before acking, the NATS server redelivers the message after the `ack_wait` timeout.

---

## Publish Mode Comparison

| Mode | Function | JetStream | Ack | Blocking | Best for |
|------|----------|-----------|-----|----------|----------|
| Sync | `nats_publish` | Yes | Wait | Yes (1 RTT) | Guaranteed delivery |
| Core | `nats_publish_core` | No | None | No | Maximum throughput, no persistence needed |
| No-ack | `nats_publish_noack` | Yes | Drop | No | High throughput with server persistence |
| Async | `nats_publish_async` | Yes | Batched | No (per-msg) | High throughput with batched confirmation |

---

## Dependencies

```toml
[dependencies]
async-nats = "0.37"     # Async NATS client
tokio = "1"              # Async runtime (full features)
futures = "0.3"          # join_all for batched ack collection
kxkdb = "0.1.0"          # kdb+ C API bindings (api feature)
libc = "0.2"             # Unix pipe for sd1 callback mechanism
```

---

## Project Structure

```
nats-rust-kdbx/
├── Cargo.toml                # [lib] cdylib — produces libkdb_plugin.so
├── Cargo.lock
├── Dockerfile.rust           # Multi-stage build (rust:1.88-slim → ubuntu:22.04)
├── nats-server.conf          # NATS JetStream server config (8MB max payload)
├── src/
│   └── lib.rs                # Plugin source — 9 FFI-exported functions
└── examples/
    ├── docker-compose.yml    # Minimal NATS JetStream stack
    ├── publisher_sync.q      # JetStream sync ack publish
    ├── publisher_noack.q     # Core NATS + JetStream no-ack publish
    ├── publisher_async.q     # Pipelined ack publish with flush
    └── subscriber.q          # JetStream durable pull subscriber
```

---

## Notes

- The NATS connection is established once per process and shared across all API calls
- `nats_subscribe` can only be called once per process (single global callback and pipe)
- The durable consumer persists its position on the NATS server — delete it with `nats consumer rm STREAM CONSUMER` to replay from the start
- kdb+ symbols (`k.h` C API) are resolved at `dlopen` time — the Rust `.so` does not link against kdb+ at compile time
