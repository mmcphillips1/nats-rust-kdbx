/ publisher_async.q — Pipelined JetStream publish with batched ack collection
/
/ Demonstrates: nats_publish_async, nats_flush, nats_set_flush_size
/
/ nats_publish_async queues the server ack future without waiting.
/ Call nats_flush[(::)] after the publish loop to await all acks in parallel.
/ Optionally set nats_set_flush_size[N] to auto-flush every N messages.

/ ── Load plugin ──────────────────────────────────────────────────────────────
PLUGIN: `$getenv[`PLUGIN_PATH],"/libkdb_plugin"
if[PLUGIN~`;PLUGIN:`:/app/target/release/libkdb_plugin];

nats_connect:        PLUGIN 2: (`nats_connect;        2)
jetstream_init:      PLUGIN 2: (`jetstream_init;      2)
nats_publish_async:  PLUGIN 2: (`nats_publish_async;  2)
nats_flush:          PLUGIN 2: (`nats_flush;           1)
nats_set_flush_size: PLUGIN 2: (`nats_set_flush_size; 1)

/ ── Connect & create stream ──────────────────────────────────────────────────
NATS_HOST: `$$[""~h:getenv`NATS_HOST; "localhost"; h]
nats_connect[NATS_HOST; 4222i]
jetstream_init[`TRADES; `trades.tick]

/ ── Sample data ──────────────────────────────────────────────────────────────
genTrade:{([] time:x#.z.p; sym:x?`AAPL`MSFT`GOOG; price:x?100f; size:x?1000i)}

/ ── Example 1: Manual flush after loop ───────────────────────────────────────
-1 "Example 1: Publishing 100 messages, manual flush at end...";
do[100; nats_publish_async[`trades.tick; genTrade 10]];
nats_flush[(::)]
-1 "All acks confirmed.";

/ ── Example 2: Auto-flush every 50 messages ──────────────────────────────────
-1 "Example 2: Publishing 200 messages with auto-flush every 50...";
nats_set_flush_size[50]
do[200; nats_publish_async[`trades.tick; genTrade 10]];
nats_flush[(::)]   / flush any remaining (200 mod 50 = 0, but good practice)
nats_set_flush_size[0]  / disable auto-flush
-1 "Done.";

exit 0
