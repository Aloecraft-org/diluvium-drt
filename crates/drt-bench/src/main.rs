//! `drt-bench` — diluvium's `swarm_bench` scenarios, run against the Rust
//! swarm port.
//!
//! Same scenarios, same flags, same JSON field names as
//! `test/swarm_bench.c`, so a run diffs directly against
//! `bench/c-swarm_bench-baseline.json`. Read `bench/README.md` for what is
//! genuinely comparable and what differs by construction.
//!
//! The doctrine is carried over verbatim: **counts and byte figures are
//! comparable; times are advisory.** A shared runner varies by more than
//! most regressions worth catching, so wall-clock alone cannot tell a slower
//! build from a busier machine. **Nothing is asserted** — this prints, and
//! fails only when a scenario stops making progress inside its deadline.

mod guests;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;

use drt_caps::Grant;
use drt_config::Budget;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::swarm::{StepHost, Swarm};
use drt_swarm::InstanceId;

/// Counts allocations so "allocation churn" can be measured rather than
/// asserted. The C harness's equivalent number is zero on the timed path:
/// it encodes its payload once before the loop and drains with a borrowed
/// `dv_queue_peek`, so every allocation counted here is one the C does not
/// make.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// RSS at process start, captured before any scenario runs.
///
/// `rss_bytes_per_agent` read from a whole-suite run is contaminated: by
/// the time `density` runs, earlier scenarios have left pages resident, and
/// the figure silently absorbs them. Reporting the delta from this baseline
/// alongside the absolute total makes the contamination visible instead.
static RSS_BASELINE: AtomicU64 = AtomicU64::new(0);

fn allocs() -> (u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

type Case = BTreeMap<String, f64>;
type Swm = Swarm<StepHost>;
type Scenario = fn(&Args, Duration) -> Result<Case, Stalled>;

#[derive(Parser)]
#[command(
    name = "drt-bench",
    about = "The swarm benchmark, against the Rust port"
)]
struct Args {
    /// Machine-readable output: one JSON object with a `cases` map.
    #[arg(long)]
    json: bool,
    /// Multiply every scenario's default sizes by this.
    #[arg(long, default_value_t = 1.0)]
    scale: f64,
    /// Run one scenario.
    #[arg(long)]
    only: Option<String>,
    /// RNG seed for churn traffic; fix it and a run reproduces exactly.
    #[arg(long, default_value_t = 7)]
    seed: u64,
    /// Arm the instruction hook and report VM instructions per op. A
    /// separate run by design: arming the count hook slows the timed path,
    /// so counted and uncounted timings are not comparable.
    #[arg(long)]
    count: bool,
    /// Wall-clock cap per scenario before it is called stalled.
    #[arg(long, default_value_t = 120.0)]
    deadline: f64,
    /// Run each scenario this many times and report the field-wise median.
    ///
    /// Single samples do not resolve the message path on a shared runner:
    /// the 16-byte round trip was measured spanning 3.88-6.75 us across five
    /// back-to-back runs of an identical binary, so any difference smaller
    /// than that band is noise wearing a number. Deterministic fields are
    /// unaffected — their median is their value.
    #[arg(long, default_value_t = 1)]
    repeat: usize,
}

/// A scenario that stopped making progress. Not a slow machine — a stall.
struct Stalled(String);

fn engine() -> Arc<DiluviumEngine> {
    Arc::new(DiluviumEngine::new().expect("the linked diluvium speaks dv ABI v1"))
}

/// Resident set size, for the figure that sits *outside* the guest heap.
fn rss_bytes() -> f64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0.0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0.0);
            return kb * 1024.0;
        }
    }
    0.0
}

/// Step until `done` or the deadline. A scenario that never satisfies its
/// condition is a stall, and a benchmark without a deadline turns the worst
/// class of defect into a job that never finishes.
fn step_until(
    sw: &mut Swm,
    what: &str,
    deadline: Duration,
    mut done: impl FnMut(&mut Swm) -> bool,
) -> Result<usize, Stalled> {
    let started = Instant::now();
    let mut steps = 0;
    while !done(sw) {
        if started.elapsed() > deadline {
            return Err(Stalled(format!(
                "{what}: no progress in {:.1}s ({steps} steps, {} alive)",
                deadline.as_secs_f64(),
                sw.alive()
            )));
        }
        sw.step();
        steps += 1;
    }
    Ok(steps)
}

fn worker_ids(sw: &Swm, root: InstanceId) -> Vec<InstanceId> {
    sw.ids().into_iter().filter(|id| *id != root).collect()
}

/// Bring `n` workers up under a supervisor and return the roster.
fn fanout(
    worker: &str,
    n: usize,
    rate: u32,
    wake: bool,
    deadline: Duration,
) -> Result<(Swm, InstanceId, usize), Stalled> {
    let mut sw = Swarm::with_limits(engine(), StepHost, (n + 8) as u32, rate);
    let src = guests::supervisor(n, worker, &["queue:work/*", "queue:log"], 0, wake);
    let root = sw
        .root(
            src.as_bytes(),
            vec![
                Grant::grant("lifecycle"),
                Grant::grant("queue:work/*"),
                Grant::grant("queue:log"),
            ],
            Budget::default(),
        )
        .map_err(|e| Stalled(format!("the supervisor would not start: {e}")))?;
    let steps = step_until(&mut sw, "fanout", deadline, |sw| sw.alive() > n)?;
    Ok((sw, root, steps))
}

// ---------------------------------------------------------------- density --

/// Spawn N idle agents and measure what each costs awake, what each costs
/// parked, and how long the transition takes in each direction.
fn density(a: &Args, deadline: Duration) -> Result<Case, Stalled> {
    let n = scaled(a, 512);
    let rss_before = rss_bytes();
    let (mut sw, root, _) = fanout(guests::WORKER_IDLE, n, 64, false, deadline)?;
    let workers = worker_ids(&sw, root);

    let mut resident_now = 0.0;
    let mut resident_peak = 0.0;
    for id in &workers {
        if let Some(inst) = sw.instance_mut(*id) {
            let u = inst.usage();
            resident_now += u.bytes_now as f64;
            resident_peak += (u.memory_kb_peak * 1024) as f64;
        }
    }
    let supervisor_bytes = sw
        .instance_mut(root)
        .map(|i| i.usage().bytes_now as f64)
        .unwrap_or(0.0);
    // Absolute, not a delta: on the second and later repetitions the pages
    // are already resident, so a delta reads zero and its median lies. The
    // process total is what a capacity plan actually pays anyway.
    let _ = rss_before;
    let rss_total = rss_bytes();
    let rss_baseline = RSS_BASELINE.load(Ordering::Relaxed) as f64;

    let started = Instant::now();
    let mut hibernated: f64 = 0.0;
    for id in &workers {
        if sw.hibernate(*id).is_ok() {
            hibernated += 1.0;
        }
    }
    let hibernate_us_each = started.elapsed().as_secs_f64() * 1e6 / n as f64;
    let cached: f64 = workers.iter().map(|id| sw.cached_size(*id) as f64).sum();

    let started = Instant::now();
    for id in &workers {
        let _ = sw.wake(*id);
    }
    let wake_us_each = started.elapsed().as_secs_f64() * 1e6 / n as f64;

    let per_agent = resident_now / n as f64;
    let cached_each = cached / hibernated.max(1.0);
    let mut c = Case::new();
    c.insert("agents".into(), n as f64);
    c.insert("slot_table_bytes_per_slot".into(), Swm::slot_bytes() as f64);
    // Unlike dvs.c, the table grows to what is claimed rather than to the
    // bound, so this is the figure a capacity plan actually pays.
    c.insert(
        "slot_table_bytes_allocated".into(),
        (sw.slots_allocated() * Swm::slot_bytes()) as f64,
    );
    c.insert("resident_bytes_per_agent".into(), per_agent);
    c.insert("resident_peak_per_agent".into(), resident_peak / n as f64);
    c.insert("supervisor_bytes".into(), supervisor_bytes);
    c.insert("rss_bytes_total".into(), rss_total);
    c.insert("rss_bytes_per_agent".into(), rss_total / n as f64);
    // What *this* scenario added, which is the figure that means something
    // when the suite has been running for a while.
    c.insert("rss_baseline_bytes".into(), rss_baseline);
    c.insert(
        "rss_bytes_over_baseline_per_agent".into(),
        (rss_total - rss_baseline).max(0.0) / n as f64,
    );
    c.insert("hibernated".into(), hibernated);
    c.insert("cached_bytes_per_agent".into(), cached_each);
    c.insert(
        "resident_over_cached".into(),
        per_agent / cached_each.max(1.0),
    );
    c.insert("hibernate_us_each".into(), hibernate_us_each);
    c.insert("wake_us_each".into(), wake_us_each);
    c.insert(
        "wake_over_hibernate".into(),
        wake_us_each / hibernate_us_each.max(f64::MIN_POSITIVE),
    );
    c.insert(
        "agents_per_GiB_resident".into(),
        (1024.0 * 1024.0 * 1024.0) / per_agent.max(1.0),
    );
    c.insert(
        "agents_per_GiB_cached".into(),
        (1024.0 * 1024.0 * 1024.0) / cached_each.max(1.0),
    );
    Ok(c)
}

// ------------------------------------------------------------------ spawn --

/// A supervisor brings N workers up through lifecycle messages, timed end to
/// end, at two program sizes and two rate limits; then the whole subtree is
/// killed from the top.
fn spawn(a: &Args, deadline: Duration) -> Result<Case, Stalled> {
    let n = scaled(a, 512);
    let large = guests::worker_large(3657);
    let mut c = Case::new();
    for (prefix, worker, rate) in [
        ("small", guests::WORKER_IDLE, 64u32),
        ("large", large.as_str(), 64),
        ("rate8", guests::WORKER_IDLE, 8),
    ] {
        let started = Instant::now();
        let (mut sw, root, steps) = fanout(worker, n, rate, false, deadline)?;
        let elapsed = started.elapsed().as_secs_f64();
        c.insert(format!("{prefix}_spawns_per_s"), n as f64 / elapsed);
        c.insert(format!("{prefix}_us_per_spawn"), elapsed * 1e6 / n as f64);
        c.insert(format!("{prefix}_steps"), steps as f64);
        c.insert(format!("{prefix}_source_bytes"), worker.len() as f64);

        let started = Instant::now();
        sw.kill(root).ok();
        let kill = started.elapsed().as_secs_f64();
        c.insert(format!("{prefix}_subtree_kill_ms"), kill * 1e3);
        c.insert(
            format!("{prefix}_subtree_kill_us_each"),
            kill * 1e6 / (n + 1) as f64,
        );
    }
    Ok(c)
}

// ------------------------------------------------------------------ queue --

/// One message in and its reply out: a host push, a guest `queue.wait`, a
/// guest push, a host drain. This is the host path — the swarm layer has no
/// guest-to-guest routing.
fn queue(a: &Args, deadline: Duration) -> Result<Case, Stalled> {
    let agents = scaled(a, 64);
    let rounds = scaled(a, 64);
    let mut c = Case::new();
    c.insert("agents".into(), agents as f64);
    c.insert("rounds".into(), rounds as f64);

    for size in [16usize, 256, 4096] {
        let (mut sw, root, _) = fanout(guests::WORKER_ECHO, agents, 64, false, deadline)?;
        let workers = worker_ids(&sw, root);
        // Encoded once, before the clock starts — the C does the same, and
        // re-encoding per message measured this harness rather than the
        // swarm.
        let mut payload = Vec::new();
        rmpv::encode::write_value(&mut payload, &rmpv::Value::Binary(vec![0x5a; size]))
            .expect("a bench payload encodes");
        let mut refused = 0.0;
        let mut delivered = 0usize;

        let (allocs_before, bytes_before) = allocs();
        // Split the count by phase, so "where do the allocations come from"
        // is answered rather than guessed at.
        let mut push_allocs = 0u64;
        let mut step_allocs = 0u64;
        let mut drain_allocs = 0u64;
        let started = Instant::now();
        for _ in 0..rounds {
            let mark = allocs().0;
            for id in &workers {
                if sw.push(*id, "work", &payload).is_ok() {
                    delivered += 1;
                } else {
                    refused += 1.0;
                }
            }
            push_allocs += allocs().0 - mark;
            // One step carries every echo through wait → push.
            let mark = allocs().0;
            sw.step();
            step_allocs += allocs().0 - mark;
            let mark = allocs().0;
            for id in &workers {
                if let Some(inst) = sw.instance_mut(*id) {
                    if let Some(q) = inst.queue("done") {
                        while matches!(inst.pop(q), Ok(Some(_))) {}
                    }
                }
            }
            drain_allocs += allocs().0 - mark;
            if started.elapsed() > deadline {
                return Err(Stalled(format!(
                    "queue: payload {size} exceeded the deadline"
                )));
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        let (allocs_after, bytes_after) = allocs();
        let trips = delivered as f64;
        c.insert(
            format!("p{size}_allocs_per_roundtrip"),
            (allocs_after - allocs_before) as f64 / trips,
        );
        c.insert(format!("p{size}_allocs_push"), push_allocs as f64 / trips);
        c.insert(format!("p{size}_allocs_step"), step_allocs as f64 / trips);
        c.insert(format!("p{size}_allocs_drain"), drain_allocs as f64 / trips);
        c.insert(
            format!("p{size}_alloc_bytes_per_roundtrip"),
            (bytes_after - bytes_before) as f64 / trips,
        );
        c.insert(format!("p{size}_roundtrips_per_s"), trips / elapsed);
        c.insert(format!("p{size}_us_per_roundtrip"), elapsed * 1e6 / trips);
        c.insert(
            format!("p{size}_MiB_per_s"),
            // Both directions: the payload goes in and the echo comes back,
            // which is what the C harness counts.
            (trips * size as f64 * 2.0) / elapsed / (1024.0 * 1024.0),
        );
        c.insert(format!("p{size}_refused_pushes"), refused);
    }
    Ok(c)
}

// ------------------------------------------------------------------- step --

/// The cost of one step with every agent parked and nothing to do, against
/// several table sizes. In `dvs.c` this walks every slot whether or not it
/// is in use, which is why its published 64×-headroom figure is labelled as
/// one that does not travel; here the table grows to what is claimed, so the
/// headroom rows should barely move.
fn step_cost(a: &Args, deadline: Duration) -> Result<Case, Stalled> {
    let n = scaled(a, 256);
    let mut c = Case::new();
    let reps = 32;
    for (prefix, agents, headroom) in [
        ("tight_table", n, 1usize),
        ("table_8x", n, 8),
        ("table_64x", n, 64),
        ("quarter_agents", n / 4, 1),
    ] {
        let mut sw = Swarm::with_limits(engine(), StepHost, ((agents + 8) * headroom) as u32, 64);
        let src = guests::supervisor(
            agents,
            guests::WORKER_IDLE,
            &["queue:work/*", "queue:log"],
            0,
            false,
        );
        let root = sw
            .root(
                src.as_bytes(),
                vec![
                    Grant::grant("lifecycle"),
                    Grant::grant("queue:work/*"),
                    Grant::grant("queue:log"),
                ],
                Budget::default(),
            )
            .map_err(|e| Stalled(format!("step: {e}")))?;
        let _ = root;
        step_until(&mut sw, "step fanout", deadline, |sw| sw.alive() > agents)?;

        // Everything parked, nothing to do: this is fixed overhead.
        let started = Instant::now();
        for _ in 0..reps {
            sw.step();
        }
        let us_per_step = started.elapsed().as_secs_f64() * 1e6 / reps as f64;
        c.insert(format!("{prefix}_us_per_step"), us_per_step);
        c.insert(
            format!("{prefix}_us_per_step_per_agent"),
            us_per_step / agents.max(1) as f64,
        );
        c.insert(
            format!("{prefix}_slots_allocated"),
            sw.slots_allocated() as f64,
        );
    }
    Ok(c)
}

// ----------------------------------------------------------------- roster --

/// The cost of resolving instance handles, which is a linear scan of the
/// slot table on every call that takes one.
fn roster(a: &Args, deadline: Duration) -> Result<Case, Stalled> {
    let n = scaled(a, 256);
    let (sw, root, _) = fanout(guests::WORKER_IDLE, n, 64, false, deadline)?;
    let ids = sw.ids();
    let reps = 1000;

    let started = Instant::now();
    let mut sink = 0usize;
    for _ in 0..reps {
        for id in &ids {
            if sw.resident(*id) {
                sink += 1;
            }
        }
    }
    let per_walk = started.elapsed().as_secs_f64() * 1e6 / reps as f64;
    std::hint::black_box(sink);

    // Derived from the walk, not measured against one handle — which is how
    // the C harness derives it (`elapsed / reps / n`). Timing a single
    // *last* handle instead measures the worst position against the C's
    // average, and reports a ~2x gap that is entirely the choice of index.
    let per_lookup = per_walk / ids.len().max(1) as f64;
    let _ = root;

    let mut c = Case::new();
    c.insert("agents".into(), n as f64);
    c.insert("us_per_full_walk".into(), per_walk);
    c.insert("us_per_lookup".into(), per_lookup);
    Ok(c)
}

// ------------------------------------------------------------------ driver --

fn scaled(a: &Args, base: usize) -> usize {
    ((base as f64 * a.scale).round() as usize).max(1)
}

/// Field-wise median over repeated runs. A field absent from some runs
/// takes the median of the runs that carry it.
fn median_of(runs: Vec<Case>) -> Case {
    let mut merged: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for run in runs {
        for (k, v) in run {
            merged.entry(k).or_default().push(v);
        }
    }
    merged
        .into_iter()
        .map(|(k, mut vs)| {
            vs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (k, vs[vs.len() / 2])
        })
        .collect()
}

fn main() -> ExitCode {
    let a = Args::parse();
    RSS_BASELINE.store(rss_bytes() as u64, Ordering::Relaxed);
    let deadline = Duration::from_secs_f64(a.deadline);
    let scenarios: Vec<(&str, Scenario)> = vec![
        ("density", density),
        ("spawn", spawn),
        ("queue", queue),
        ("step", step_cost),
        ("roster", roster),
    ];

    let mut cases: BTreeMap<String, Case> = BTreeMap::new();
    for (name, run) in scenarios {
        if let Some(only) = &a.only {
            if only != name {
                continue;
            }
        }
        if !a.json {
            eprintln!("== {name}");
        }
        let mut runs = Vec::with_capacity(a.repeat.max(1));
        let mut stalled = None;
        for _ in 0..a.repeat.max(1) {
            match run(&a, deadline) {
                Ok(case) => runs.push(case),
                Err(e) => {
                    stalled = Some(e);
                    break;
                }
            }
        }
        match stalled.map_or_else(|| Ok(median_of(runs)), Err) {
            Ok(case) => {
                if !a.json {
                    for (k, v) in &case {
                        eprintln!("   {k} = {v:.4}");
                    }
                }
                cases.insert(name.into(), case);
            }
            Err(Stalled(why)) => {
                eprintln!("drt-bench: {name} stalled: {why}");
                return ExitCode::FAILURE;
            }
        }
    }

    if a.json {
        let out = serde_json::json!({
            "tool": "drt_bench",
            "scale": a.scale,
            "seed": a.seed,
            "counted": a.count,
            "repeat": a.repeat,
            "cases": cases,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    }
    ExitCode::SUCCESS
}
