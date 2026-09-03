//! The time connector.
//!
//! Wall-clock milliseconds since the epoch — the one nondeterminism every
//! program eventually wants, and the reason the hostcall shape wins: the
//! answer arrives as a message, so it is in the log, so a replay replays the
//! same moment instead of the replayer's.
//!
//! The surface and semantics mirror `conn_time` in diluvium's `host/dhost.c`
//! (a guest must not be able to tell hosts apart):
//!
//! - `time` — wall-clock ms since the Unix epoch.
//! - `time/monotonic` — ms, deliberately the same unit, on this host
//!   process's own epoch: good for intervals within a run, reset by a restart
//!   or a restore, never comparable to a persisted wall timestamp. Intervals
//!   belong here, records belong on `time`.

use drt_platform::clock::{self, Instant};

use drt_caps::Scope;
use drt_connector::{CallError, CallResult, Connector};

pub struct TimeConnector {
    /// The process epoch for `time/monotonic`.
    started: Instant,
}

impl TimeConnector {
    pub fn new() -> Self {
        TimeConnector {
            started: Instant::now(),
        }
    }
}

impl Default for TimeConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Connector for TimeConnector {
    async fn call(
        &self,
        call: &str,
        _args: Option<rmpv::Value>,
        _scope: Option<&Scope>,
    ) -> CallResult {
        match call {
            "time" => {
                let ms = clock::wall_ms().map_err(|e| CallError::new(e.to_string()))?;
                Ok(rmpv::Value::from(ms))
            }
            "time/monotonic" => Ok(rmpv::Value::from(self.started.elapsed().as_millis() as u64)),
            other => Err(CallError::new(format!(
                "the time connector answers 'time' and 'time/monotonic'; '{other}' is neither"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drt_caps::{CapSet, Grant};
    use drt_connector::{mock::MockConnector, Dispatcher, Registry};
    use drt_hostcall::{to_bytes, Request, Status};
    use std::sync::Arc;

    #[test]
    fn answers_the_documented_surface() {
        let c = TimeConnector::new();
        let wall = pollster::block_on(c.call("time", None, None)).unwrap();
        // A plausible wall clock: after 2020, an unsigned integer of ms.
        assert!(wall.as_u64().unwrap() > 1_577_836_800_000);
        let a = pollster::block_on(c.call("time/monotonic", None, None)).unwrap();
        let b = pollster::block_on(c.call("time/monotonic", None, None)).unwrap();
        assert!(b.as_u64().unwrap() >= a.as_u64().unwrap());
        let err = pollster::block_on(c.call("time/warp", None, None)).unwrap_err();
        assert!(err.to_string().contains("neither"));
    }

    /// The acceptance property (SPEC.md §7): same guest bytes, mock backing
    /// vs real backing, and the replies are shape-identical — a guest cannot
    /// tell. Only the ok-value differs, which is the point of a mock.
    #[test]
    fn guest_cannot_tell_mock_from_real() {
        let caps = CapSet::root(vec![Grant::grant("host:time*")]);
        let request = to_bytes(&Request {
            tok: 7,
            call: "time".into(),
            args: None,
        })
        .unwrap();

        let mut real = Registry::new();
        real.wire("time", Arc::new(TimeConnector::new()), None)
            .unwrap();
        let mut mocked = Registry::new();
        mocked
            .wire(
                "time",
                Arc::new(MockConnector::new().answer("time", rmpv::Value::from(12_345u64))),
                None,
            )
            .unwrap();

        for registry in [real, mocked] {
            let reply = pollster::block_on(Dispatcher::new(registry).dispatch(&caps, &request));
            assert_eq!(reply.tok, Some(7));
            assert_eq!(reply.status, Status::Ok);
            assert!(reply.value.as_ref().unwrap().is_u64());
            assert_eq!(reply.detail, None);
        }
    }
}
