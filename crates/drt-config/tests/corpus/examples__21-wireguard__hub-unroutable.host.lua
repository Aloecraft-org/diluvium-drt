-- A hub: it announces a subnet that lives behind it.
--
--   drt wg check --config hub-unroutable.host.lua
--
-- Nothing here is wrong, and nothing here will work. DRT gives the
-- interface `address` and the kernel derives exactly one route from it --
-- the on-link one for 10.9.0.0/24. The peer below is allowed a network
-- outside that, so WireGuard would encrypt for it happily and nothing
-- would ever hand it a packet.
--
-- That is the failure `drt wg check` exists to name: a tunnel that comes
-- up, reports a handshake, and carries nothing.
return {
  wireguard = {
    listen_port = 51820,
    interface   = "drt0",
    address     = "10.9.0.1/24",
    private_key = "8nq7WxIN1FhAoPljA+Te1djRmDWpN/WZto6OTwktZnU=",
    peers = {
      {
        public_key  = "p6VqzzX91j0TRajLGT9Dlkn0TEvvk+j4m9E38LlXhSQ=",
        -- Reachable: inside the interface's own prefix.
        -- Not reachable: everything behind the hub, until a route exists.
        allowed_ips = { "10.9.0.2/32", "192.168.1.0/24" },
        endpoint    = "203.0.113.7:51820",
      },
    },
  },
}
