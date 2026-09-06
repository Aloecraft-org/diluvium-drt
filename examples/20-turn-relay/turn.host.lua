-- The TURN relay: the last rung of the traversal ladder.
--
--   drt turn --config turn.host.lua
--
-- `netcheck` says whether a direct path can exist and `stun` measures the
-- mapping that decides it. When the answer is no — a symmetric NAT, a
-- browser with no UDP — something has to carry the traffic, and this is it.
-- It costs bandwidth, which is why it is the last rung and not the first.
return {
  turn = {
    bind          = "127.0.0.1",   -- faces the world in a real deployment
    port          = 18493,
    relay_address = "127.0.0.1",   -- the address peers are told to send to

    -- The same secret as `connectors.crypto.turn` in app.host.lua, and that
    -- is the whole deployment: this server verifies what that connector
    -- mints. Real ones come from `openssl rand -hex 32`; from a file or an
    -- environment variable in anything but an example (key_file, key_env).
    key = "the-coturn-static-auth-secret-1",

    realm = "example",

    -- example: omits queue/report_ms — name them and `drt start` carries the
    -- counters and every allocation's closing byte count, with its principal,
    -- to the root program, which is what a meter reads
  },
}
