-- The rendezvous relay, on loopback so this example needs one machine.
--
--   drt relay --config rendezvous.host.lua
--
-- In a real deployment `bind` faces an edge that terminates TLS and routes
-- <label>--tunnel.<zone> here, and the URLs below are wss://. Nothing about
-- the relay changes; it carries bytes either way.
--
-- Two keys per label, because two parties hold them: the park key lives on
-- the device, the caller key is what you hand out. Real ones come from
-- `openssl rand -hex 24`; these are fixed so the example is reproducible.
return {
  relay = {
    bind = "127.0.0.1",
    port = 18490,
    labels = {
      fp = {
        park_key   = "park-key-for-the-example-only",
        caller_key = "caller-key-for-the-example-only",
      },
    },
  },
}
