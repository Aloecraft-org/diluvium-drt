-- The rendezvous relay: a public address that holds parked WebSocket legs
-- and splices the two carrying the same label into one byte pipe.
--
--   drt relay --config rendezvous.host.lua
--
-- Two keys per label, because two parties hold them: the park key lives on
-- the device forever, the caller key is what you hand out. Both are blank
-- below, and a blank key is refused when this file loads.
return {
  relay = {
    bind = "0.0.0.0",
    port = 8443,

    -- example: omits queue/reply_queue — name them and `drt start` asks the
    -- deployment before it admits a leg, where a quota or a revoked device
    -- can say no and silence inside admit_timeout_ms is itself a refusal.

    labels = {
      xps = {
        park_key   = "",   -- openssl rand -hex 24
        caller_key = "",   -- openssl rand -hex 24, a different one
      },
    },
  },
}
