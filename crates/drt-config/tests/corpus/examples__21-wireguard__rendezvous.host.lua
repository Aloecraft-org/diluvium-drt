-- A rendezvous fetchpoint: it learns its peers at run time.
--
--   drt wg check --config rendezvous.host.lua
--
-- No `peers` at all, and that is the point. A device that can only talk
-- to peers already written into its config is a device that never needed
-- a rendezvous. This one comes up knowing nobody, measures its own NAT
-- mapping against the two STUN servers, publishes what it finds, and
-- waits to be told about a peer on `reply_queue`.
--
-- Two servers, never one: one can report an address, only two can say
-- whether it CHANGED between them, which is what decides whether a punch
-- is possible at all.
return {
  wireguard = {
    listen_port = 51820,
    interface   = "drt-fp",
    address     = "10.9.0.1/24",
    private_key = "8nq7WxIN1FhAoPljA+Te1djRmDWpN/WZto6OTwktZnU=",
    stun        = { "stun.l.google.com:19302", "stun1.l.google.com:19302" },
    queue       = "wg_in",   -- the mapping, and every change, arrive here
    reply_queue = "wg_out",  -- `add` and `remap` are read here
    peers       = {},        -- learnt later, over the rendezvous
  },
}
-- example: omits the supervisor that reads wg_in and writes wg_out; that
-- program is the rendezvous, and doc/WireGuard.md §2 has its shape.
