//! The browser tier driven end to end, without a browser.
//!
//! A mock [`HostBridge`] fakes a diluvium instance — queues, a park, an
//! echo on resume — well enough to drive a real
//! [`drt_swarm::swarm::Swarm`]. That exercises the whole chain the browser
//! uses (`Swarm` → `JsHost` → bridge → instance ops) with ordinary `cargo
//! test`, which is the entire reason the JS contract is a Rust trait rather
//! than an `extern` block.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use drt_config::Budget;
use drt_swarm::engine::{PushOutcome, QueueHandle, QueueStatus, Step, UsageReport, WaitSet};
use drt_swarm::swarm::Swarm;
use drt_web::{BrowserEngine, Driven, HostBridge, InstanceHandle, JsHost};

/// A fake guest: declares `in` and `out`, parks on `in`, and on resume
/// moves one message across. The shape of `WORKER_ECHO`, in Rust.
#[derive(Default)]
struct FakeInstance {
    queues: Vec<(String, Vec<Vec<u8>>)>,
    parked: bool,
    done: bool,
    resumes: u64,
}

impl FakeInstance {
    fn q(&mut self, name: &str) -> usize {
        if let Some(i) = self.queues.iter().position(|(n, _)| n == name) {
            return i;
        }
        self.queues.push((name.to_string(), Vec::new()));
        self.queues.len() - 1
    }
}

#[derive(Clone, Default)]
struct MockBridge {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    instances: HashMap<InstanceHandle, FakeInstance>,
    next: InstanceHandle,
    released: Vec<InstanceHandle>,
    drives: u64,
}

impl MockBridge {
    fn released(&self) -> Vec<InstanceHandle> {
        self.inner.lock().unwrap().released.clone()
    }
    fn drives(&self) -> u64 {
        self.inner.lock().unwrap().drives
    }
    fn out_of(&self, h: InstanceHandle) -> Vec<Vec<u8>> {
        let mut st = self.inner.lock().unwrap();
        let inst = st.instances.get_mut(&h).expect("instance");
        let i = inst.q("out");
        inst.queues[i].1.clone()
    }
}

impl HostBridge for MockBridge {
    fn abi_version(&self) -> u32 {
        1
    }

    fn load(
        &self,
        _program: &str,
        _name: &str,
        _budget: Budget,
        _unsafe_stdlib: bool,
    ) -> Result<InstanceHandle, String> {
        let mut st = self.inner.lock().unwrap();
        st.next += 1;
        let h = st.next;
        st.instances.insert(h, FakeInstance::default());
        Ok(h)
    }

    fn restore(
        &self,
        _snapshot: &[u8],
        _host_stamp: Option<&str>,
        _budget: Budget,
        _unsafe_stdlib: bool,
    ) -> Result<InstanceHandle, String> {
        self.load("", "", Budget::default(), false)
    }

    fn release(&self, instance: InstanceHandle) {
        let mut st = self.inner.lock().unwrap();
        st.instances.remove(&instance);
        st.released.push(instance);
    }

    fn queue(&self, instance: InstanceHandle, name: &str) -> Option<QueueHandle> {
        let mut st = self.inner.lock().unwrap();
        let inst = st.instances.get_mut(&instance)?;
        Some(QueueHandle(inst.q(name) as u32 + 1))
    }

    fn queue_info(
        &self,
        instance: InstanceHandle,
        queue: QueueHandle,
    ) -> Result<QueueStatus, String> {
        let st = self.inner.lock().unwrap();
        let inst = st.instances.get(&instance).ok_or("no instance")?;
        let (_, msgs) = inst
            .queues
            .get(queue.0 as usize - 1)
            .ok_or("no such queue")?;
        Ok(QueueStatus {
            len: msgs.len() as u32,
            capacity: 8,
            enabled: true,
            exported: true,
        })
    }

    fn push(
        &self,
        instance: InstanceHandle,
        queue: QueueHandle,
        msgpack: &[u8],
    ) -> Result<PushOutcome, String> {
        let mut st = self.inner.lock().unwrap();
        let inst = st.instances.get_mut(&instance).ok_or("no instance")?;
        let (_, msgs) = inst
            .queues
            .get_mut(queue.0 as usize - 1)
            .ok_or("no such queue")?;
        if msgs.len() >= 8 {
            return Ok(PushOutcome::Full);
        }
        msgs.push(msgpack.to_vec());
        Ok(PushOutcome::Accepted)
    }

    fn pop(&self, instance: InstanceHandle, queue: QueueHandle) -> Result<Option<Vec<u8>>, String> {
        let mut st = self.inner.lock().unwrap();
        let inst = st.instances.get_mut(&instance).ok_or("no instance")?;
        let (_, msgs) = inst
            .queues
            .get_mut(queue.0 as usize - 1)
            .ok_or("no such queue")?;
        Ok(if msgs.is_empty() {
            None
        } else {
            Some(msgs.remove(0))
        })
    }

    fn run(&self, instance: InstanceHandle) -> Result<Step, String> {
        let mut st = self.inner.lock().unwrap();
        let inst = st.instances.get_mut(&instance).ok_or("no instance")?;
        let inq = inst.q("in");
        inst.q("out");
        inst.parked = true;
        Ok(Step::Parked(WaitSet::new(
            [QueueHandle(inq as u32 + 1)],
            None,
            false,
        )))
    }

    fn resume(&self, instance: InstanceHandle, _fired: QueueHandle) -> Result<Step, String> {
        let mut st = self.inner.lock().unwrap();
        let inst = st.instances.get_mut(&instance).ok_or("no instance")?;
        inst.resumes += 1;
        let inq = inst.q("in");
        let msg = if inst.queues[inq].1.is_empty() {
            None
        } else {
            Some(inst.queues[inq].1.remove(0))
        };
        if let Some(msg) = msg {
            let outq = inst.q("out");
            inst.queues[outq].1.push(msg);
        }
        Ok(Step::Parked(WaitSet::new(
            [QueueHandle(inq as u32 + 1)],
            None,
            false,
        )))
    }

    fn resume_timeout(&self, instance: InstanceHandle) -> Result<Step, String> {
        self.resume(instance, QueueHandle(1))
    }

    fn current_wait(&self, instance: InstanceHandle) -> Option<WaitSet> {
        let mut st = self.inner.lock().unwrap();
        let inst = st.instances.get_mut(&instance)?;
        if !inst.parked || inst.done {
            return None;
        }
        let inq = inst.q("in");
        Some(WaitSet::new([QueueHandle(inq as u32 + 1)], None, false))
    }

    fn usage(&self, _instance: InstanceHandle) -> UsageReport {
        UsageReport {
            instructions: 0,
            memory_kb_peak: 0,
            bytes_now: 0,
        }
    }

    fn exceeded(&self, _instance: InstanceHandle) -> bool {
        false
    }

    fn snapshot(
        &self,
        _instance: InstanceHandle,
        _host_stamp: Option<&str>,
    ) -> Result<Vec<u8>, String> {
        Ok(b"fake-snapshot".to_vec())
    }

    /// What JS does during a step: find a waited queue with a message and
    /// resume on it. The same logic `StepHost` runs natively, here standing
    /// in for `js_host_drive`.
    fn drive(&self, _id: u32, instance: InstanceHandle) -> Driven {
        self.inner.lock().unwrap().drives += 1;
        let wait = match self.current_wait(instance) {
            None => return self.step_to_driven(self.run(instance)),
            Some(w) => w,
        };
        for q in wait.queues() {
            if self
                .queue_info(instance, *q)
                .map(|i| i.len > 0)
                .unwrap_or(false)
            {
                return self.step_to_driven(self.resume(instance, *q));
            }
        }
        Driven::Alive
    }
}

impl MockBridge {
    fn step_to_driven(&self, step: Result<Step, String>) -> Driven {
        match step {
            Ok(Step::Parked(_)) => Driven::Alive,
            Ok(Step::Done) => Driven::Exited,
            Err(e) => Driven::Faulted(e),
        }
    }
}

fn swarm(bridge: MockBridge) -> Swarm<JsHost<MockBridge>> {
    Swarm::new(
        Arc::new(BrowserEngine::new(bridge.clone())),
        JsHost::new(bridge),
    )
}

/// The whole chain: a real `Swarm` builds an instance through the browser
/// engine, drives it through the JS host, and a message round-trips — all
/// of it across the bridge.
#[test]
fn a_message_round_trips_through_the_browser_engine() {
    let bridge = MockBridge::default();
    let mut sw = swarm(bridge.clone());
    let root = sw
        .root(b"-- a fake program", vec![], Budget::default())
        .expect("the browser engine should build a root");
    assert_eq!(sw.alive(), 1);

    sw.step(); // runs to its park
    sw.push(root, "in", b"\x01").expect("push");
    sw.step(); // JS drive finds the message and resumes

    let handle = 1; // the first instance the bridge minted
    assert_eq!(bridge.out_of(handle), vec![b"\x01".to_vec()]);
    assert!(bridge.drives() >= 2, "the JS host was driven");
}

/// Dropping an instance releases the JS-side entry. Without this a swarm
/// that hibernates and kills leaks the JS table for the page's lifetime.
#[test]
fn killing_an_instance_releases_the_js_handle() {
    let bridge = MockBridge::default();
    let mut sw = swarm(bridge.clone());
    let root = sw.root(b"-- fake", vec![], Budget::default()).unwrap();
    sw.step();
    assert!(bridge.released().is_empty());

    sw.kill(root).expect("kill");
    assert_eq!(bridge.released(), vec![1], "JS was told to release it");
}

/// The browser tier has no bytecode verifier either, so a precompiled
/// chunk is refused before it reaches JS — the same refusal every host
/// makes, in one place.
#[test]
fn bytecode_is_refused_before_it_reaches_js() {
    use drt_swarm::engine::{Engine, LoadSpec, ProgramBytes};
    let engine = BrowserEngine::new(MockBridge::default());
    // `Box<dyn Instance>` is not Debug, so match rather than unwrap_err.
    let err = match engine.load(LoadSpec {
        program: ProgramBytes::Bytecode(b"\x1bLua"),
        name: "chunk",
        budget: Budget::default(),
        unsafe_stdlib: false,
    }) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("bytecode was accepted"),
    };
    assert!(err.contains("source only"), "{err}");
    assert!(err.contains("verifier"), "{err}");
}

/// Capability gating stays reachable from the browser: `holds` is what the
/// panel and an app both ask before offering an action.
#[test]
fn capability_gating_survives_the_bridge() {
    use drt_caps::Grant;
    let bridge = MockBridge::default();
    let mut sw = swarm(bridge);
    let root = sw
        .root(
            b"-- fake",
            vec![Grant::grant("host:time"), Grant::grant("queue:*")],
            Budget::default(),
        )
        .unwrap();
    assert!(sw.holds(root, "host:time"));
    assert!(!sw.holds(root, "host:fs/read"));
}
