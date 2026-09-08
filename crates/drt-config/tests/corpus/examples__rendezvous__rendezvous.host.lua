-- A rendezvous fetchpoint: `drt start` with a relay in it.
--
--   drt --config rendezvous.host.lua start
--
-- Generate real keys before this reaches anything public —
--   openssl rand -hex 24
-- — one per leg per label, and never the same value for both. The park key
-- lives on the device forever; the caller key is what you hand out.
return {
  supervisor = "supervisor.lua",

  relay = {
    bind = "0.0.0.0",
    port = 8443,

    -- Where the supervisor hears about the relay. Naming `reply_queue` is
    -- what opts this deployment in to being ASKED before a leg proceeds —
    -- and having opted in, a question it fails to answer within
    -- admit_timeout_ms is a refusal. Delete the line and the static keys
    -- become the only gate again.
    queue = "relay_in",
    reply_queue = "relay_out",
    admit_timeout_ms = 2000,

    labels = {
      xps = {
        park_key   = "REPLACE-ME-park-0000000000000000",
        caller_key = "REPLACE-ME-call-0000000000000000",
      },
    },
  },
}
