/ publisher_noack.q — Fire-and-forget publishing
/
/ Demonstrates two no-ack publish modes:
/   nats_publish_core  — core NATS (no JetStream). Fastest, no persistence guarantee.
/   nats_publish_noack — JetStream publish, but drops the ack future immediately.
/                        Message IS persisted by the server; publisher just doesn't wait.
/
/ Both return immediately without blocking. Choose based on whether you need
/ server-side persistence (noack) or pure speed with no JetStream overhead (core).

/ ── Load plugin ──────────────────────────────────────────────────────────────
PLUGIN: `$getenv[`PLUGIN_PATH],"/libkdb_plugin"
if[PLUGIN~`;PLUGIN:`:/app/target/release/libkdb_plugin];

nats_connect:       PLUGIN 2: (`nats_connect;       2)
jetstream_init:     PLUGIN 2: (`jetstream_init;     2)
nats_publish_core:  PLUGIN 2: (`nats_publish_core;  2)
nats_publish_noack: PLUGIN 2: (`nats_publish_noack; 2)

/ ── Connect ──────────────────────────────────────────────────────────────────
NATS_HOST: `$$[""~h:getenv`NATS_HOST; "localhost"; h]
nats_connect[NATS_HOST; 4222i]

/ JetStream stream needed only for nats_publish_noack (not for core NATS)
jetstream_init[`TRADES; `trades.tick]

/ ── Sample data ──────────────────────────────────────────────────────────────
genTrade:{([] time:x#.z.p; sym:x?`AAPL`MSFT`GOOG; price:x?100f; size:x?1000i)}

/ ── Core NATS: fire-and-forget (no JetStream, no persistence) ────────────────
-1 "Publishing 10 messages via core NATS (fire-and-forget)...";
do[10; nats_publish_core[`trades.tick; genTrade 5]];
-1 "Core NATS: 10 messages sent (no ack, no persistence guarantee).";

/ ── JetStream no-ack: persisted but ack dropped ─────────────────────────────
-1 "Publishing 10 messages via JetStream no-ack...";
do[10; nats_publish_noack[`trades.tick; genTrade 5]];
-1 "JetStream no-ack: 10 messages sent (persisted, ack dropped).";

exit 0
