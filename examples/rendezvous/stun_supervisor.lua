-- The supervisor of a STUN server. A binding server is stateless and
-- answers strangers by design, so there is nothing to arbitrate here the
-- way the relay arbitrates legs — only counters to watch.
--
-- The one reading worth acting on: `dropped` climbing while `requests`
-- does not is scanner traffic, not clients. The server drops non-binding
-- datagrams in silence rather than answering them (an unconditional reply
-- would make it a reflector for spoofed traffic), so this counter is how
-- you see that happening at all.

local events = queue.declare('stun_in', {capacity = 64})

print('supervisor: watching stun')
while true do
  local _, m = queue.wait({events})
  if m.event == 'stun' then
    print(string.format(
      'stun %s: requests=%d responses=%d dropped=%d in=%dB out=%dB',
      m.addr, m.requests, m.responses, m.dropped, m.bytes_in, m.bytes_out))

    -- Scanners outnumbering clients is worth knowing about.
    if m.dropped > m.requests and m.dropped > 100 then
      print('stun: more junk than binding requests — check what is aimed at this port')
    end
  end
end
