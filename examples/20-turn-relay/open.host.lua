-- The same relay with no key. A TURN server with nothing to verify against
-- is an open relay — bandwidth for anyone who finds it — so it refuses to
-- bind rather than starting and hoping.
return {
  turn = {
    bind          = "127.0.0.1",
    port          = 18493,
    relay_address = "127.0.0.1",
    realm         = "example",
  },
}
