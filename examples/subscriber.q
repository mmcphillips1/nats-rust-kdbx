/ subscriber.q — JetStream durable pull subscriber
/
/ Demonstrates: nats_connect, jetstream_init, nats_subscribe
/ The subscriber runs a background consumer thread; the callback fires on q's
/ main thread via the sd1/pipe event-loop mechanism.

/ ── Load plugin ──────────────────────────────────────────────────────────────
PLUGIN: `$getenv[`PLUGIN_PATH],"/libkdb_plugin"
if[PLUGIN~`;PLUGIN:`:/app/target/release/libkdb_plugin];

nats_connect:   PLUGIN 2: (`nats_connect;   2)
jetstream_init: PLUGIN 2: (`jetstream_init; 2)
nats_subscribe: PLUGIN 2: (`nats_subscribe; 5)

/ ── Connect & ensure stream exists ───────────────────────────────────────────
NATS_HOST: `$$[""~h:getenv`NATS_HOST; "localhost"; h]
nats_connect[NATS_HOST; 4222i]
jetstream_init[`TRADES; `trades.tick]

/ ── In-memory table to accumulate received messages ──────────────────────────
trades:([] time:`timestamp$(); sym:`$(); price:`float$(); size:`int$())

/ ── Callback: fires once per NATS message on q's main thread ────────────────
/ The callback receives the exact K type that was published.
/ Tables arrive as tables, dicts as dicts, atoms as atoms.
on_trade:{[msg]
    $[98h = type msg;                          / table
        `trades upsert msg;
      99h = type msg;                          / dict (single row)
        `trades upsert enlist msg;
      -1 "Received non-table message: ",(.Q.s msg)
    ];
    -1 "on_trade | rows in trades: ",string count trades;
    }

/ ── Subscribe ────────────────────────────────────────────────────────────────
/ deliver_policy options:
/   `all              — replay entire stream from the beginning
/   `last             — start from the last message in the stream
/   `new              — only receive messages published after subscribing
/   `last_per_subject — one message per unique subject (latest)

nats_subscribe[`TRADES; `trades.tick; `my_consumer; `all; `on_trade]

-1 "Subscriber ready — waiting for messages on trades.tick";
-1 "Consumer: my_consumer | Policy: all (replay from start)";
-1 "Press Ctrl+C to exit.";
