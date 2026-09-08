-- The deployment that mints credentials. It does not run the relay; it
-- shares one secret with it.
return {
  supervisor = "app.dlua",
  caps = { "host:crypto/turn_credential" },
  connectors = {
    crypto = {
      key = "a-master-key-for-this-example-000",
      turn = {
        key  = "the-coturn-static-auth-secret-1",  -- turn.host.lua's, exactly
        ttl  = 3600,
        uris = { "turn:127.0.0.1:18493?transport=udp" },
      },
    },
  },
}
