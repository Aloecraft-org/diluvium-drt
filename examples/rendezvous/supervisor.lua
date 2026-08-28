-- The supervisor of a rendezvous fetchpoint. It does the three things the
-- relay cannot do for itself, all over one queue: knows who is home,
-- counts what they moved, and decides who gets in.
--
-- Everything below is ordinary Lua on ordinary messages. That is the
-- point: policy lives here, where you can change it without touching the
-- runtime, and the relay stays a byte pipe with a key check.

local events  = queue.declare('relay_in',  {capacity = 256})
local answers = queue.declare('relay_out', {capacity = 256})

-- Who is home, and what they have moved. A panel reads this; so does a
-- bill. Both are just tables, because presence and metering were never
-- two features — they are two readings of one event stream.
local home, moved = {}, {}

-- Policy. Replace this with whatever your deployment actually knows:
-- a tenant lookup, a maintenance window, a revocation list, a quota.
-- It is asked before EVERY leg, park and caller alike, so it is also
-- where you turn a label off without restarting anything.
local function admit(label, leg)
  if label == 'revoked' then
    return false
  end
  -- A crude quota, as an example of the thing static keys cannot express:
  -- once a label has moved 1 GiB, stop admitting callers to it. The device
  -- may still park, so it comes back the moment you raise the number.
  if leg == 'caller' and (moved[label] or 0) > 1024 * 1024 * 1024 then
    return false
  end
  return true
end

print('supervisor: watching the relay')
while true do
  local _, m = queue.wait({events})

  if m.event == 'admit' then
    local ok = admit(m.label, m.leg)
    -- Answer naming the token we were asked with. Silence is a refusal,
    -- so this push is not optional.
    queue.push(answers, {tok = m.tok, ok = ok})
    if not ok then
      print(string.format('supervisor: refused %s/%s', m.label, m.leg))
    end

  elseif m.event == 'parked' then
    home[m.label] = true
    print(string.format('supervisor: %s is home', m.label))

  elseif m.event == 'claimed' then
    print(string.format('supervisor: %s session %d opened', m.label, m.session))

  elseif m.event == 'closed' then
    moved[m.label] = (moved[m.label] or 0) + m.bytes
    print(string.format('supervisor: %s session %d carried %d bytes (%d total)',
                        m.label, m.session, m.bytes, moved[m.label]))
  end
end
