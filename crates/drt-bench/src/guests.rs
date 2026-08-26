//! The guest programs, carried over from diluvium's `test/swarm_bench.c` so
//! the two harnesses measure the same workload. Changing one without the
//! other silently makes the numbers incomparable, which is the whole point
//! of the exercise — so they are transcribed rather than rewritten.

/// Asks for `n` workers through `system/lifecycle`, then parks. The
/// supervisor's own cost is reported separately, never folded into the
/// per-agent figure.
pub fn supervisor(n: usize, worker: &str, caps: &[&str], budget: u64, wake: bool) -> String {
    let caps = caps
        .iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(", ");
    // A long bracket long enough that no worker source can close it.
    format!(
        "local sys  = queue.declare('system/lifecycle', {{capacity = {cap}}})\n\
         local ev   = queue.declare('system/events', {{capacity = 256}})\n\
         local log  = queue.declare('log', {{capacity = 8, exported = true}})\n\
         local hold = queue.declare('hold', {{capacity = 1}})\n\
         local WORKER = [====[{worker}]====]\n\
         local asked = 0\n\
         for i = 1, {n} do\n\
         \x20 if queue.push(sys, {{op = 'spawn', code = WORKER,\n\
         \x20                     caps = {{{caps}}},\n\
         \x20                     budget = {{instructions = {budget}}},\n\
         \x20                     wake_on_message = {wake}}}) then\n\
         \x20   asked = asked + 1\n\
         \x20 end\n\
         end\n\
         queue.push(log, 'asked:' .. asked)\n\
         queue.wait({{hold}})\n",
        cap = n.max(8),
    )
}

/// The smallest agent that is still an agent: an inbox, and a loop that
/// reads it. Everything the density figure measures beyond this is the
/// interpreter.
pub const WORKER_IDLE: &str = "local inbox = queue.declare('work', {capacity = 4})\n\
     local done  = queue.declare('done', {capacity = 8, exported = true})\n\
     local seen = 0\n\
     while true do\n\
       local id, v = queue.wait({inbox})\n\
       seen = seen + 1\n\
       queue.push(done, seen)\n\
     end\n";

/// An agent that echoes what it is sent, for measuring what a queue costs.
pub const WORKER_ECHO: &str = "local inbox = queue.declare('work', {capacity = 8})\n\
     local done  = queue.declare('done', {capacity = 8, exported = true})\n\
     while true do\n\
       local id, v = queue.wait({inbox})\n\
       queue.push(done, v)\n\
     end\n";

/// Padding to reach the C bench's "large program" size (3,657 bytes) without
/// changing what the program does — the spawn scenario measures the cost of
/// carrying source through a lifecycle message, so only the byte count
/// matters.
pub fn worker_large(target_bytes: usize) -> String {
    let mut src = String::from(WORKER_IDLE);
    let comment_overhead = 3; // "-- " per line
    while src.len() + comment_overhead + 40 < target_bytes {
        src.push_str("-- padding to the reference program's source size\n");
    }
    while src.len() < target_bytes {
        src.push_str("--\n");
    }
    src
}
