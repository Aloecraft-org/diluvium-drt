# 16-exec

A local command from inside the sandbox. `exec/run` takes a vector, answers
with the exit status and both streams, and the deployment bounds it three
ways, because the instruction budget cannot reach a subprocess.

## Run it

```
cd examples/16-exec
drt run --config deploy.json
```

## What you should see

```
$ drt run --config deploy.json
drt: exec wired: granting host:exec/run leaves the sandbox (GUARANTEES.md); only the scope's deadline, output cap and allow list bound it
a vector, never a shell string; the exit is an answer:
  echo hello             ok      status 0  hello
  sh -c 'exit 3'         ok      status 3
  cat, fed stdin         ok      status 0  from the program
  no-such-program        ok      status 127

what the scope bounds, and in whose words:
  sleep 5, timeout 200   error   the child was killed at the 200 ms deadline
  sleep 5, timeout 5000  error   timeout_ms passed the host's ceiling (2000 ms); a call may ask for less, never more
  yes, until the cap     error   stdout passed this deployment's byte cap (4096); the child was killed, the output refused
  ls, not allowed        error   'ls' is outside this scope's allow list; a program may start only what the deployment allows
```

The run exits 0. The first line is stderr, printed when the config loads and
before the program starts: wiring `exec` is announced, never implied.

## What it teaches

**A vector, never a shell string.** `argv` is handed to exec as a list, so
there is no string a quote can escape from. A program that wants a shell
asks for one by name, visibly: `{ "sh", "-c", ... }`.

**The exit is an answer.** `status 3` is what the child said, read the way a
shell script reads `$?`, and a program that does not exist is `127`, the
shell's own convention. `error` is reserved for the call itself failing.

**Three bounds, all the deployment's.** A call may ask for a shorter deadline
than `max_timeout_ms` and never a longer one; past the deadline the child and
everything it started are killed. `max_output_bytes` caps each stream and
stdin, and past it the output is refused rather than truncated. `allow`
names the programs a call may start, by absolute path, and anything else is
refused by name. Leave `allow` out and any program on `PATH` may run, which
is what the C host does; a `.host.lua` saying `exec = true` loads unchanged.

**It leaves the sandbox, and drt says so.** The connector is in the `full`
build only, off until a config names it, and announced on stderr when one
does. What a command does once it runs is outside every promise in
`GUARANTEES.md`, which is why the scope is the whole of the safety story.
