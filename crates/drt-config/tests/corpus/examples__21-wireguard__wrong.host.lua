-- Three things a config can get wrong, and what each is answered with.
-- Uncomment one at a time; the file as it stands trips the first.
return {
  wireguard = {
    listen_port = 51820,
    interface   = "drt-fp",
    private_key = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaA=",

    -- A route's network address where a host address belongs. The commonest
    -- transcription slip, and it yields an interface that answers to nothing.
    address = "10.9.0.0/24",

    -- listen_port = 0,          -- cannot be named to a peer or measured
    -- stun = { "stun1.example:3478" },  -- one server cannot classify a NAT
  },
}
