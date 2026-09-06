-- One end of a WireGuard tunnel: the hub.
--
--   drt wg --config hub.host.lua
--
-- `wg-quick`'s field names, so an [Interface]/[Peer] stanza transcribes
-- rather than translates, and a key pasted from one works in the other.
return {
  wireguard = {
    -- The port a peer dials and a NAT mapping belongs to. Required, never
    -- zero: an ephemeral port cannot be named to a peer or measured.
    listen_port = 51821,

    interface = "drt-hub",
    address   = "10.9.0.2/24",   -- the interface comes up holding this
    mtu       = 1420,            -- not 1500: WireGuard's overhead is 60-80

    -- `drt wg keygen` prints a pair. Read from the environment so the
    -- config carries the shape and never the value.
    private_key_env = "HUB_PRIVATE_KEY",

    -- example: omits `stun` — name two servers and the device measures its
    -- own mapping on listen_port before binding it, and reports the address
    -- a rendezvous should publish (doc/WireGuard.md §2)

    peers = {
      {
        public_key  = "HzHu0poKQwY2l7Nqvmtcec3y9NlqgmVCnPEhmf+TGiI=",
        allowed_ips = { "10.9.0.1/32" },
        endpoint    = "127.0.0.1:51820",
        keepalive   = 25,   -- what holds a punched mapping open
      },
    },
  },
}
