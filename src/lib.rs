/// kdb+ plugin — four APIs callable from q via 2:
///
///   nats_connect[host; port]                        (required)
///       Establish the NATS connection and cache it globally.
///       All other APIs share this connection via clone().
///
///   jetstream_init[stream_name; subject]            (optional)
///       Create the JetStream stream if it does not already exist.
///       Skip this call when the stream already exists on the server.
///
///   nats_publish[subject; bytes]
///       Publish pre-serialised bytes with JetStream ack wait (one RTT per message).
///       Pass `-8! value` from q — Rust treats the payload as opaque bytes.
///
///   nats_publish_core[subject; bytes]
///       Publish pre-serialised bytes via core NATS (no JetStream).
///       Fire-and-forget — no stream persistence or ack overhead.
///
///   nats_publish_noack[subject; bytes]
///       Publish pre-serialised bytes to JetStream; drop the ack future immediately.
///       Message is persisted but publisher does not confirm delivery.
///
///   nats_publish_async[subject; bytes]
///       Publish pre-serialised bytes to JetStream; queue the ack future for nats_flush.
///       Returns immediately — no per-message blocking.
///
///   nats_flush[(::)]
///       Await all ack futures queued by nats_publish_async in parallel.
///       Call once after the publish loop.
///
///   nats_subscribe[stream_name; subject; consumer_name; deliver_policy; callback]
///       Attach to an existing stream, start a background consumer thread,
///       and register a q event-loop callback that fires on each message.
///       Rust decodes the IPC bytes; the callback receives the decoded K value.
///       Non-blocking for q — returns immediately after registration.
///       deliver_policy symbols: `all `last `new `last_per_subject
///
/// Serialisation: handled entirely in Rust via the C API (q_ipc_encode / q_ipc_decode).
/// Publishers pass any K value directly — no `-8!` needed in q.
/// Subscribers receive the decoded K value directly — no `9!` needed in q.
///
/// Callback mechanism (sd1 / register_callback — mirrors kxkdb plumber example):
///   1. nats_subscribe creates a Unix pipe and registers the read end with q's
///      event loop via register_callback (wraps sd1).
///   2. A background OS thread runs a dedicated current_thread tokio runtime,
///      pulling messages from NATS.
///   3. Each PendingMsg (decoded K + NATS Message) is boxed and its pointer
///      written (8 bytes) to the pipe write end.
///   4. q's event loop calls nats_msg_handler on q's own main thread, which
///      reads the pointer, invokes callback[decoded_K], and acks the NATS
///      message only if the callback succeeds (at-least-once delivery).

use async_nats::jetstream::{self, consumer::{pull, DeliverPolicy}, stream};
use futures::future::join_all;
use futures::StreamExt;
use kxkdb::api::native;
use kxkdb::api::*;
use kxkdb::qtype;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use tokio::runtime::Runtime;

/// Bundles a decoded K value with its NATS message handle so the ack can be
/// deferred until after the q callback succeeds.
struct PendingMsg {
    decoded_k: K,
    msg: async_nats::jetstream::message::Message,
}

// ── Globals ───────────────────────────────────────────────────────────────────

static RT:            OnceLock<Runtime>                              = OnceLock::new();
static CLIENT:        OnceLock<async_nats::Client>                   = OnceLock::new();
static NATS_URL:      OnceLock<String>                               = OnceLock::new();
static CALLBACK:      OnceLock<std::ffi::CString>                    = OnceLock::new();
static PIPE_WRITE_FD: AtomicI32                                      = AtomicI32::new(-1);
static ACK_FLUSH_SIZE: AtomicUsize                                    = AtomicUsize::new(0);
// Queued ack futures for nats_publish_async / nats_flush pipelined publish.
// PublishAckFuture implements IntoFuture (not Future directly), so we box each
// one into a type-erased Future<Output=bool> at the point of storage.
type AckFut = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;
static PENDING_ACKS: Mutex<Vec<AckFut>> = Mutex::new(Vec::new());

// ── Internal helpers ──────────────────────────────────────────────────────────

fn rt() -> &'static Runtime {
    RT.get_or_init(|| Runtime::new().unwrap())
}

/// Returns the cached NATS client. Panics if nats_connect has not been called.
/// Must NOT be called from inside an async context (uses block_on internally).
fn nats_client() -> &'static async_nats::Client {
    CLIENT.get_or_init(|| {
        let url = NATS_URL
            .get()
            .expect("[kdb-plugin] call nats_connect[host; port] before using NATS APIs");
        println!("[kdb-plugin] connecting to NATS at {}", url);
        rt()
            .block_on(async_nats::connect(url.as_str()))
            .expect("[kdb-plugin] NATS connect failed")
    })
}

// ── Pipe callback — runs on q's main thread via q's event loop ────────────────

extern "C" fn nats_msg_handler(pipe_read_fd: I) -> K {
    // Read the 8-byte PendingMsg pointer written by the consumer thread.
    let mut ptr: *mut PendingMsg = std::ptr::null_mut();
    unsafe {
        libc::read(
            pipe_read_fd,
            &mut ptr as *mut *mut PendingMsg as *mut libc::c_void,
            std::mem::size_of::<*mut PendingMsg>(),
        );
    }

    if ptr.is_null() {
        return KNULL;
    }

    let pending = unsafe { Box::from_raw(ptr) };

    // Invoke the registered q callback: callback_fn[decoded_K]
    let cb = CALLBACK
        .get()
        .expect("[kdb-plugin] nats_msg_handler: no callback registered");
    let result = error_to_string(unsafe {
        native::k(0, cb.as_ptr() as S, pending.decoded_k, KNULL)
    });

    if result.get_type() == qtype::ERROR {
        eprintln!(
            "[kdb-plugin] callback error: {}",
            result.get_error_string().unwrap_or("unknown")
        );
        decrement_reference_count(result);
        // Don't ack — message will be redelivered after ack_wait timeout
        return KNULL;
    }

    decrement_reference_count(result);

    // Callback succeeded — ack the message
    if let Err(e) = rt().block_on(pending.msg.ack()) {
        eprintln!("[kdb-plugin] ack error: {}", e);
    }

    KNULL
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Establish the NATS connection. Must be called before any other API.
/// Subsequent calls are no-ops — the connection is cached for the process lifetime.
///
///   nats_connect: PLUGIN 2: (`nats_connect; 2)
///   nats_connect[`localhost; 4222i]
///   nats_connect[`my-nats-server; 4222i]
#[no_mangle]
pub extern "C" fn nats_connect(host: K, port: K) -> K {
    let host_str = host.get_symbol().unwrap_or("localhost");
    let port_num = port.get_int().unwrap_or(4222);
    let url      = format!("nats://{}:{}", host_str, port_num);

    // Store the URL so nats_client() can use it on first connect.
    // ok() silently ignores a second call — connection is already established.
    if NATS_URL.set(url.clone()).is_ok() {
        // Force the connection now so any failure surfaces at connect-time, not later.
        let _ = nats_client();
        println!("[kdb-plugin] nats_connect | connected to {}", url);
    } else {
        println!("[kdb-plugin] nats_connect | already connected (ignored)");
    }

    KNULL
}

/// Create the JetStream stream if it does not already exist on the server.
/// Optional — skip this call when the stream is pre-configured on the server.
/// Requires nats_connect to have been called first.
///
///   jetstream_init: PLUGIN 2: (`jetstream_init; 2)
///   jetstream_init[`TRADES; `trades.tick]
#[no_mangle]
pub extern "C" fn jetstream_init(stream_name: K, subject: K) -> K {
    let sname = match stream_name.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("jetstream_init: stream_name is required\0"),
    };
    let subj = match subject.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("jetstream_init: subject is required\0"),
    };

    rt().block_on(async {
        let js = jetstream::new(nats_client().clone());
        js.get_or_create_stream(stream::Config {
            name:     sname.clone(),
            subjects: vec![subj.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    });

    println!(
        "[kdb-plugin] jetstream_init | stream={} subject={}",
        sname, subj
    );
    KNULL
}

/// Publish any kdb+ object to the given subject as kdb+ IPC binary.
/// Serialisation is handled internally via q_ipc_encode (C API) — pass any K value.
/// Requires nats_connect to have been called first.
///
///   nats_publish: PLUGIN 2: (`nats_publish; 2)
///   nats_publish[`trades.tick; trade]
#[no_mangle]
pub extern "C" fn nats_publish(subject: K, payload: K) -> K {
    let subject_str = match subject.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("nats_publish: subject is required\0"),
    };

    let bytes_k = match payload.q_ipc_encode(3) {
        Ok(b)  => b,
        Err(e) => {
            eprintln!("[kdb-plugin] nats_publish | encode error: {}", e);
            return KNULL;
        }
    };
    let payload: Vec<u8> = bytes_k.as_mut_slice::<G>().to_vec();
    decrement_reference_count(bytes_k);

    let result = rt().block_on(async {
        let js = jetstream::new(nats_client().clone());
        js.publish(subject_str.clone(), payload.into())
            .await?
            .await?; // wait for JetStream server ack
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    if let Err(e) = result {
        eprintln!("[kdb-plugin] nats_publish | error: {}", e);
    }

    KNULL
}

/// Publish any kdb+ object via core NATS (no JetStream).
/// Serialisation is handled internally via q_ipc_encode (C API) — pass any K value.
/// Fire-and-forget — no stream persistence or ack overhead on the publisher side.
/// If a JetStream stream is configured to capture the subject, messages are still
/// persisted by the server (but the publisher is unaware).
/// Requires nats_connect to have been called first.
///
///   nats_publish_core: PLUGIN 2: (`nats_publish_core; 2)
///   nats_publish_core[`bench.rust; trade]
#[no_mangle]
pub extern "C" fn nats_publish_core(subject: K, payload: K) -> K {
    let subject_str = match subject.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("nats_publish_core: subject is required\0"),
    };

    let bytes_k = match payload.q_ipc_encode(3) {
        Ok(b)  => b,
        Err(e) => {
            eprintln!("[kdb-plugin] nats_publish_core | encode error: {}", e);
            return KNULL;
        }
    };
    let payload: Vec<u8> = bytes_k.as_mut_slice::<G>().to_vec();
    decrement_reference_count(bytes_k);

    let result = rt().block_on(async {
        nats_client()
            .publish(subject_str.clone(), payload.into())
            .await?;
        Ok::<(), async_nats::Error>(())
    });

    if let Err(e) = result {
        eprintln!("[kdb-plugin] nats_publish_core | error: {}", e);
    }
    KNULL
}

/// Publish any kdb+ object to JetStream without waiting for the server ack.
/// Serialisation is handled internally via q_ipc_encode (C API) — pass any K value.
/// The message is sent and persisted on the server; the ack future is dropped.
/// Use when throughput matters more than per-message delivery confirmation.
/// Requires nats_connect + jetstream_init to have been called first.
///
///   nats_publish_noack: PLUGIN 2: (`nats_publish_noack; 2)
///   nats_publish_noack[`bench.rust; trade]
#[no_mangle]
pub extern "C" fn nats_publish_noack(subject: K, payload: K) -> K {
    let subject_str = match subject.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("nats_publish_noack: subject is required\0"),
    };

    let bytes_k = match payload.q_ipc_encode(3) {
        Ok(b)  => b,
        Err(e) => {
            eprintln!("[kdb-plugin] nats_publish_noack | encode error: {}", e);
            return KNULL;
        }
    };
    let payload: Vec<u8> = bytes_k.as_mut_slice::<G>().to_vec();
    decrement_reference_count(bytes_k);

    let result = rt().block_on(async {
        let js = jetstream::new(nats_client().clone());
        let _ack_future = js.publish(subject_str.clone(), payload.into()).await?;
        // ack future dropped — message sent, no wait for server confirmation
        Ok::<(), async_nats::Error>(())
    });

    if let Err(e) = result {
        eprintln!("[kdb-plugin] nats_publish_noack | error: {}", e);
    }
    KNULL
}

/// Publish any kdb+ object to JetStream and queue the ack future for later collection.
/// Serialisation is handled internally via q_ipc_encode (C API) — pass any K value.
/// Returns immediately — does not wait for the server ack.
/// Call nats_flush[(::)] after the publish loop to await all queued acks in parallel.
/// Requires nats_connect + jetstream_init to have been called first.
///
///   nats_publish_async: PLUGIN 2: (`nats_publish_async; 2)
///   do[N; nats_publish_async[`bench.rust; trade]]
///   nats_flush[(::)]
#[no_mangle]
pub extern "C" fn nats_publish_async(subject: K, payload: K) -> K {
    let subject_str = match subject.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("nats_publish_async: subject is required\0"),
    };

    let bytes_k = match payload.q_ipc_encode(3) {
        Ok(b)  => b,
        Err(e) => {
            eprintln!("[kdb-plugin] nats_publish_async | encode error: {}", e);
            return KNULL;
        }
    };
    let payload: Vec<u8> = bytes_k.as_mut_slice::<G>().to_vec();
    decrement_reference_count(bytes_k);

    let ack_future = rt().block_on(async {
        let js = jetstream::new(nats_client().clone());
        js.publish(subject_str.clone(), payload.into()).await
    });

    match ack_future {
        Ok(f)  => {
            let boxed: AckFut = Box::pin(async move { f.await.is_ok() });
            let mut pending = PENDING_ACKS.lock().unwrap();
            pending.push(boxed);

            let flush_size = ACK_FLUSH_SIZE.load(Ordering::Relaxed);
            if flush_size > 0 && pending.len() >= flush_size {
                let futures: Vec<AckFut> = std::mem::take(&mut *pending);
                drop(pending);
                let results: Vec<bool> = rt().block_on(join_all(futures));
                let n_err = results.iter().filter(|&&ok| !ok).count();
                if n_err > 0 {
                    eprintln!("[kdb-plugin] nats_publish_async | auto-flush: {}/{} acks failed", n_err, results.len());
                }
            }
        }
        Err(e) => { eprintln!("[kdb-plugin] nats_publish_async | publish error: {}", e); }
    }
    KNULL
}

/// Set the auto-flush threshold for nats_publish_async.
/// When N > 0, nats_publish_async will automatically flush after every N queued acks.
/// Set to 0 (default) to disable auto-flush and rely on manual nats_flush calls.
///
///   nats_set_flush_size: PLUGIN 2: (`nats_set_flush_size; 1)
///   nats_set_flush_size[1000]
#[no_mangle]
pub extern "C" fn nats_set_flush_size(n: K) -> K {
    let size = match n.get_long() {
        Ok(v) => v as usize,
        Err(_) => match n.get_int() {
            Ok(v) => v as usize,
            Err(_) => {
                eprintln!("[kdb-plugin] nats_set_flush_size | expected int or long");
                return KNULL;
            }
        }
    };
    ACK_FLUSH_SIZE.store(size, Ordering::Relaxed);
    println!("[kdb-plugin] nats_set_flush_size | auto-flush after {} acks{}", size,
             if size == 0 { " (disabled)" } else { "" });
    KNULL
}

/// Await all ack futures queued by nats_publish_async and drain the queue.
/// Blocks until every queued ack is confirmed by the server (or fails).
/// Prints a summary of how many acks succeeded and how many failed.
///
///   nats_flush: PLUGIN 2: (`nats_flush; 1)
///   nats_flush[(::)]
#[no_mangle]
pub extern "C" fn nats_flush(_dummy: K) -> K {
    let futures = std::mem::take(&mut *PENDING_ACKS.lock().unwrap());
    let n = futures.len();
    if n == 0 {
        println!("[kdb-plugin] nats_flush | no pending acks");
        return KNULL;
    }

    let results: Vec<bool> = rt().block_on(join_all(futures));
    let n_err = results.iter().filter(|&&ok| !ok).count();

    if n_err == 0 {
        println!("[kdb-plugin] nats_flush | {} acks confirmed", n);
    } else {
        eprintln!("[kdb-plugin] nats_flush | {}/{} acks failed", n_err, n);
    }
    KNULL
}

/// Attach to an existing JetStream stream and start a background consumer.
/// On each message, decodes the IPC bytes and calls callback[decoded_K] on q's
/// main thread via q's event loop. Rust decodes using q_ipc_decode (C API).
/// Returns immediately — non-blocking for q.
/// Requires nats_connect to have been called first. The stream must already exist
/// (call jetstream_init or ensure it is pre-configured on the server).
///
///   nats_subscribe: PLUGIN 2: (`nats_subscribe; 5)
///   on_trade: {[t] show t; `trades upsert t}
///   nats_subscribe[`TRADES; `trades.tick; `my_consumer; `all; `on_trade]
///   nats_subscribe[`TRADES; `trades.tick; `my_consumer; `new; `on_trade]
///
/// deliver_policy: `all | `last | `new | `last_per_subject  (required)
#[no_mangle]
pub extern "C" fn nats_subscribe(stream_name: K, subject: K, consumer_name: K, deliver_policy: K, callback: K) -> K {
    let stream_name_str = match stream_name.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("nats_subscribe: stream_name is required\0"),
    };
    let subject_str = match subject.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("nats_subscribe: subject is required\0"),
    };
    let consumer_name_str = match consumer_name.get_symbol() {
        Ok(s) => s.to_string(),
        Err(_)  => return new_error("nats_subscribe: consumer_name is required\0"),
    };
    let policy_str = match deliver_policy.get_symbol() {
        Ok(s) => s,
        Err(_)  => return new_error("nats_subscribe: deliver_policy is required (`all `last `new `last_per_subject)\0"),
    };
    let cb_name = match callback.get_symbol() {
        Ok(s) => s,
        Err(_)  => return new_error("nats_subscribe: callback is required\0"),
    };

    let dp = match policy_str {
        "all"              => DeliverPolicy::All,
        "last"             => DeliverPolicy::Last,
        "new"              => DeliverPolicy::New,
        "last_per_subject" => DeliverPolicy::LastPerSubject,
        _                  => return new_error("nats_subscribe: unknown deliver_policy — use `all `last `new `last_per_subject\0"),
    };

    CALLBACK
        .set(std::ffi::CString::new(cb_name).unwrap())
        .expect("[kdb-plugin] nats_subscribe already called");

    // ── Unix pipe ─────────────────────────────────────────────────────────────
    // Consumer thread writes K pointer to write_fd;
    // q's event loop reads it from read_fd and calls nats_msg_handler.
    let mut fds: [I; 2] = [-1; 2];
    if 0 != unsafe { libc::pipe(fds.as_mut_ptr()) } {
        return new_error("nats_subscribe: pipe() failed\0");
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    PIPE_WRITE_FD.store(write_fd, Ordering::SeqCst);

    // Register read end with q's event loop — calls nats_msg_handler on data.
    if KNULL == register_callback(read_fd, nats_msg_handler) {
        return new_error("nats_subscribe: register_callback (sd1) failed\0");
    }

    // Pre-connect before entering any async context to avoid nested block_on.
    let cl = nats_client().clone();

    // ── Consumer thread ───────────────────────────────────────────────────────
    // Dedicated current_thread runtime: future runs on one OS thread only so K
    // raw pointers created after .await points are safe (no thread migration).
    std::thread::spawn(move || {
        let local_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        println!("[kdb-plugin] nats_subscribe | consumer thread started");

        local_rt.block_on(async move {
            let js     = jetstream::new(cl);
            let stream = js.get_stream(&stream_name_str).await.unwrap();

            let consumer = stream
                .get_or_create_consumer(
                    &consumer_name_str,
                    pull::Config {
                        durable_name:   Some(consumer_name_str.clone()),
                        filter_subject: subject_str.clone(),
                        deliver_policy: dp,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();

            println!(
                "[kdb-plugin] nats_subscribe | stream={} subject={} consumer={} policy={:?}",
                stream_name_str, subject_str, consumer_name_str, dp
            );
            let mut messages = consumer.messages().await.unwrap();

            while let Some(msg) = messages.next().await {
                let msg     = msg.unwrap();
                let payload = msg.payload.to_vec();
                // Ack deferred — handled by nats_msg_handler after q callback

                // Decode kdb+ IPC binary → K.
                // pin_symbol locks q's symbol intern table so symbols can be
                // safely created from this non-q thread (setm pattern).
                pin_symbol();

                let byte_k = new_list(qtype::BYTE_LIST, payload.len() as i64);
                byte_k.as_mut_slice::<G>().copy_from_slice(&payload);

                let decode_result = byte_k.q_ipc_decode();
                decrement_reference_count(byte_k);

                match decode_result {
                    Ok(decoded_k) => {
                        unpin_symbol();
                        // Box the decoded K with the NATS message for deferred ack.
                        // Write the pointer to the pipe (8 bytes).
                        // q's event loop detects this and calls nats_msg_handler.
                        let pending = Box::new(PendingMsg { decoded_k, msg });
                        let ptr = Box::into_raw(pending);
                        unsafe {
                            libc::write(
                                write_fd,
                                &ptr as *const *mut PendingMsg as *const libc::c_void,
                                std::mem::size_of::<*mut PendingMsg>(),
                            );
                        }
                    }
                    Err(e) => {
                        unpin_symbol();
                        eprintln!("[kdb-plugin] nats_subscribe | decode error: {}", e);
                    }
                }
            }
        });
    });

    println!(
        "[kdb-plugin] nats_subscribe | callback=`{}",
        cb_name
    );
    KNULL
}
