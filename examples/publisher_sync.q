/ publisher_sync.q — JetStream publish with server acknowledgment
/
/ Demonstrates: nats_connect, jetstream_init, nats_publish
/ Each message blocks until the server confirms persistence (one round-trip).
/ Safest option — guarantees every message is stored before returning.

/ ── Load plugin ──────────────────────────────────────────────────────────────
PLUGIN: `$getenv[`PLUGIN_PATH],"/libkdb_plugin"
if[PLUGIN~`;PLUGIN:`:/app/target/release/libkdb_plugin];

nats_connect:   PLUGIN 2: (`nats_connect;   2)
jetstream_init: PLUGIN 2: (`jetstream_init; 2)
nats_publish:   PLUGIN 2: (`nats_publish;   2)

/ ── Connect & create stream ──────────────────────────────────────────────────
NATS_HOST: `$$[""~h:getenv`NATS_HOST; "localhost"; h]
nats_connect[NATS_HOST; 4222i]
jetstream_init[`TRADES; `trades.tick]

/ ── Sample trade table ───────────────────────────────────────────────────────
trades:([]
    time:3#.z.p;
    sym:`AAPL`MSFT`GOOG;
    price:182.5 415.2 175.8;
    size:100 50 200i
    )

/ ── Publish ──────────────────────────────────────────────────────────────────
-1 "Publishing table (3 rows)...";
nats_publish[`trades.tick; trades]

-1 "Publishing single row (dict)...";
nats_publish[`trades.tick; last trades]

-1 "Publishing atom...";
nats_publish[`trades.tick; 42j]

-1 "Done — 3 messages published with JetStream ack.";
exit 0
