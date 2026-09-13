//! The state machine: many calls, one stream, polled and never blocking.
//!
//! # Surface
//!
//! Entry points:
//! - [`Session::new`] — wrap a [`Channel`](crate::channel::Channel).
//! - [`Session::begin`] — write a request, get its wire id.
//! - [`Session::poll`] — flush writes, drain reads, sort replies by id.
//!   Called from the drive loop's cadence, never waited on.
//! - [`Session::take`] — collect one call's reply if it has landed.
//! - [`Session::abandon`] — give up on a call; a later reply is dropped.
//!
//! Configurable values:
//! - `max_inflight`, per session, from the manifest or the `plugins`
//!   block. Zero is not special-cased: see [`Session::new`].
//!
//! Fan-out: the three terminal outcomes in [`SessionError`]. Every one is
//! sticky — see "why a failure is permanent" below.
//!
//! # Why the demultiplexing lives here and not in the caller
//!
//! One stream carries every call to one plugin, so the reply a caller
//! wants may arrive behind two replies it does not. A future that read
//! until it saw its own id would consume another call's answer and drop
//! it. So reads land in one table keyed by wire id, every poll, and a
//! caller only ever asks whether *its* id is there yet.
//!
//! This is the `doc/Plugins.md` §3.2 state machine: stepped, not threaded,
//! because it has to run where there are no threads.
//!
//! # Why a failure is permanent
//!
//! A framing error is not recoverable. The next length is read from the
//! bytes the previous frame's body happened to contain, so one bad frame
//! desynchronises everything after it. A session that has failed says the
//! same thing to every later call rather than appearing to recover.

use std::collections::{HashMap, HashSet};

use crate::channel::{Channel, ChannelError};
use crate::frame::{self, FrameError, Reply, ReplyBody, Request, PROTOCOL_VERSION};

/// What ends a session. Each is terminal and each is reported to every
/// call still outstanding, because none of them leave the stream usable.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("the plugin channel failed: {0}")]
    Channel(#[from] ChannelError),

    #[error("the plugin sent a frame this host cannot read: {0}")]
    Frame(#[from] FrameError),

    #[error(
        "the plugin answered call {id} with a non-final reply, and this host \
         does not consume streamed replies"
    )]
    Streamed { id: u64 },

    #[error("the plugin answered call {id}, which this host never made")]
    Unasked { id: u64 },

    #[error(
        "the plugin has {in_flight} calls outstanding, its max_inflight; \
         the call was not sent"
    )]
    Saturated { in_flight: usize },
}

/// One plugin's stream, and every call on it.
pub struct Session<C: Channel> {
    channel: C,
    /// Bytes written but not yet accepted by the channel.
    outbound: Vec<u8>,
    /// Bytes read but not yet a whole frame.
    inbound: Vec<u8>,
    /// Replies that landed and have not been collected.
    ready: HashMap<u64, Reply>,
    /// Ids sent and neither collected nor abandoned.
    live: HashSet<u64>,
    /// Ids given up on. A reply for one of these is dropped, not an error:
    /// the plugin is answering a question the host stopped waiting for.
    abandoned: HashSet<u64>,
    /// Once set, every call reports this and nothing is read or written.
    failed: Option<String>,
    max_inflight: usize,
}

impl<C: Channel> Session<C> {
    /// `max_inflight` of zero means unlimited, matching the manifest field
    /// being absent. A plugin that wants to serialise its calls declares
    /// one, and the host holds it to that rather than discovering it.
    pub fn new(channel: C, max_inflight: usize) -> Self {
        Self {
            channel,
            outbound: Vec::new(),
            inbound: Vec::new(),
            ready: HashMap::new(),
            live: HashSet::new(),
            abandoned: HashSet::new(),
            failed: None,
            max_inflight,
        }
    }

    /// How many calls are sent and unanswered.
    pub fn in_flight(&self) -> usize {
        self.live.len()
    }

    /// Queue a call. The bytes go out on this or a later [`Self::poll`];
    /// the id is usable immediately.
    pub fn begin(&mut self, target: &str, args: Option<rmpv::Value>) -> Result<u64, SessionError> {
        self.check_failed()?;
        if self.max_inflight != 0 && self.live.len() >= self.max_inflight {
            return Err(SessionError::Saturated {
                in_flight: self.live.len(),
            });
        }
        let id = frame::next_wire_id();
        let bytes = frame::encode(&Request {
            version: PROTOCOL_VERSION,
            id,
            target: target.to_string(),
            args,
        })
        .map_err(|e| self.fail(SessionError::Frame(e)))?;
        self.outbound.extend_from_slice(&bytes);
        self.live.insert(id);
        Ok(id)
    }

    /// One step: push what it can, pull what has arrived, file the frames.
    ///
    /// Does no work and reports no error once the session has failed; the
    /// failure reaches callers through [`Self::take`] and [`Self::begin`],
    /// which is where a caller can do something about it.
    pub fn poll(&mut self) -> Result<(), SessionError> {
        self.check_failed()?;
        self.flush()?;
        self.fill()?;
        self.sort()
    }

    /// The reply for `id`, if it has landed. `None` is "not yet" — the
    /// caller polls again.
    pub fn take(&mut self, id: u64) -> Result<Option<Reply>, SessionError> {
        self.check_failed()?;
        match self.ready.remove(&id) {
            Some(reply) => {
                self.live.remove(&id);
                Ok(Some(reply))
            }
            None => Ok(None),
        }
    }

    /// Stop waiting for `id`. Its reply, if it ever comes, is dropped
    /// rather than treated as an answer to a question nobody asked.
    pub fn abandon(&mut self, id: u64) {
        if self.live.remove(&id) {
            self.abandoned.insert(id);
        }
        self.ready.remove(&id);
    }

    // depth: the three halves of a poll, and the sticky failure.

    fn flush(&mut self) -> Result<(), SessionError> {
        while !self.outbound.is_empty() {
            let n = self
                .channel
                .write_some(&self.outbound)
                .map_err(|e| self.fail(SessionError::Channel(e)))?;
            if n == 0 {
                break; // the plugin is not reading; try again next poll.
            }
            self.outbound.drain(..n);
        }
        Ok(())
    }

    fn fill(&mut self) -> Result<(), SessionError> {
        loop {
            let n = self
                .channel
                .read_some(&mut self.inbound)
                .map_err(|e| self.fail(SessionError::Channel(e)))?;
            if n == 0 {
                return Ok(());
            }
        }
    }

    fn sort(&mut self) -> Result<(), SessionError> {
        loop {
            let decoded = frame::decode::<Reply>(&self.inbound)
                .map_err(|e| self.fail(SessionError::Frame(e)))?;
            let Some((reply, used)) = decoded else {
                return Ok(());
            };
            self.inbound.drain(..used);

            if reply.version != PROTOCOL_VERSION {
                return Err(self.fail(SessionError::Frame(FrameError::Version {
                    found: reply.version,
                })));
            }
            if !reply.is_final {
                return Err(self.fail(SessionError::Streamed { id: reply.id }));
            }
            if self.abandoned.remove(&reply.id) {
                continue; // answered too late; the caller is gone.
            }
            if !self.live.contains(&reply.id) {
                return Err(self.fail(SessionError::Unasked { id: reply.id }));
            }
            self.ready.insert(reply.id, reply);
        }
    }

    fn check_failed(&self) -> Result<(), SessionError> {
        match &self.failed {
            Some(why) => Err(SessionError::Channel(ChannelError::Broken(why.clone()))),
            None => Ok(()),
        }
    }

    /// Record a terminal failure and hand it back for returning.
    fn fail(&mut self, e: SessionError) -> SessionError {
        if self.failed.is_none() {
            self.failed = Some(e.to_string());
        }
        e
    }
}

/// The value or the error a finished call carries.
pub fn outcome(reply: &Reply) -> &ReplyBody {
    &reply.body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Loopback;
    use crate::frame::{ErrorClass, PluginError};

    /// Drive the far side: read whatever requests are pending and answer
    /// each one with `value`, the way the echo fixture does.
    fn answer_all(s: &mut Session<Loopback>, value: rmpv::Value) {
        let asked = s.channel.peer_take();
        let mut at = 0;
        while let Some((req, used)) = frame::decode::<Request>(&asked[at..]).unwrap() {
            at += used;
            let bytes = frame::encode(&Reply {
                version: PROTOCOL_VERSION,
                id: req.id,
                is_final: true,
                body: ReplyBody::Ok {
                    value: value.clone(),
                },
            })
            .unwrap();
            s.channel.peer_put(&bytes);
        }
    }

    #[test]
    fn a_call_goes_out_and_its_answer_comes_back() {
        let mut s = Session::new(Loopback::new(), 0);
        let id = s.begin("echo/say", Some(rmpv::Value::from("hi"))).unwrap();
        s.poll().unwrap();
        assert_eq!(s.take(id).unwrap(), None, "nothing has answered yet");

        answer_all(&mut s, rmpv::Value::from("hi"));
        s.poll().unwrap();

        let reply = s.take(id).unwrap().expect("the answer landed");
        assert_eq!(
            reply.body,
            ReplyBody::Ok {
                value: rmpv::Value::from("hi")
            }
        );
        assert_eq!(s.in_flight(), 0, "collecting it clears the slot");
    }

    /// The property the whole module exists for: an answer for one call
    /// must not be consumed by another call's wait.
    #[test]
    fn replies_arriving_out_of_order_reach_the_right_callers() {
        let mut s = Session::new(Loopback::new(), 0);
        let a = s.begin("echo/a", None).unwrap();
        let b = s.begin("echo/b", None).unwrap();
        let c = s.begin("echo/c", None).unwrap();
        s.poll().unwrap();
        s.channel.peer_take();

        // Answer them backwards, each with its own id as the value.
        for id in [c, b, a] {
            let bytes = frame::encode(&Reply {
                version: PROTOCOL_VERSION,
                id,
                is_final: true,
                body: ReplyBody::Ok {
                    value: rmpv::Value::from(id),
                },
            })
            .unwrap();
            s.channel.peer_put(&bytes);
        }
        s.poll().unwrap();

        for id in [a, b, c] {
            let reply = s.take(id).unwrap().expect("landed");
            assert_eq!(
                reply.body,
                ReplyBody::Ok {
                    value: rmpv::Value::from(id)
                },
                "call {id} got its own answer"
            );
        }
    }

    /// The backpressure path: the plugin has stopped reading, its buffer
    /// is full, and the host must neither block nor lose the request. The
    /// peer drains a little between polls, the way a busy plugin does.
    #[test]
    fn a_request_waits_out_a_plugin_that_stopped_reading() {
        let mut s = Session::new(Loopback::new(), 0);
        s.channel.capacity = Some(16);
        let id = s
            .begin("echo/say", Some(rmpv::Value::from("x".repeat(200))))
            .unwrap();

        let mut polls = 0;
        let mut drained = Vec::new();
        while !s.outbound.is_empty() {
            s.poll().unwrap();
            drained.extend_from_slice(&s.channel.peer_take());
            polls += 1;
            assert!(polls < 100, "the flush must make progress every poll");
        }
        assert!(polls > 1, "a 16-byte buffer cannot take it in one go");

        // What arrived is the request, whole and in order.
        let (req, _) = frame::decode::<Request>(&drained).unwrap().unwrap();
        assert_eq!(req.id, id, "reassembled across {polls} polls");

        let bytes = frame::encode(&Reply {
            version: PROTOCOL_VERSION,
            id,
            is_final: true,
            body: ReplyBody::Ok {
                value: rmpv::Value::from("ok"),
            },
        })
        .unwrap();
        s.channel.peer_put(&bytes);
        s.poll().unwrap();
        assert!(s.take(id).unwrap().is_some(), "and the call completes");
    }

    #[test]
    fn a_reply_arriving_one_byte_at_a_time_is_assembled() {
        let mut s = Session::new(Loopback::new(), 0);
        let id = s.begin("echo/say", None).unwrap();
        s.poll().unwrap();
        s.channel.peer_take();

        let bytes = frame::encode(&Reply {
            version: PROTOCOL_VERSION,
            id,
            is_final: true,
            body: ReplyBody::Ok {
                value: rmpv::Value::from(1),
            },
        })
        .unwrap();

        for (i, b) in bytes.iter().enumerate() {
            s.channel.peer_put(&[*b]);
            s.poll().unwrap();
            let last = i + 1 == bytes.len();
            assert_eq!(
                s.take(id).unwrap().is_some(),
                last,
                "byte {i} of {}: complete only at the end",
                bytes.len()
            );
        }
    }

    #[test]
    fn an_error_reply_is_delivered_as_an_error_not_a_failure() {
        let mut s = Session::new(Loopback::new(), 0);
        let id = s.begin("echo/nope", None).unwrap();
        s.poll().unwrap();
        s.channel.peer_take();

        let bytes = frame::encode(&Reply {
            version: PROTOCOL_VERSION,
            id,
            is_final: true,
            body: ReplyBody::Err {
                error: PluginError {
                    class: ErrorClass::Plugin,
                    code: "nope".into(),
                    message: "not today".into(),
                },
            },
        })
        .unwrap();
        s.channel.peer_put(&bytes);
        s.poll().unwrap();

        let reply = s.take(id).unwrap().expect("an error is still an answer");
        match reply.body {
            ReplyBody::Err { error } => assert_eq!(error.code, "nope"),
            other => panic!("expected an error body, got {other:?}"),
        }
        assert!(s.poll().is_ok(), "the session is still usable");
    }

    #[test]
    fn a_late_reply_for_an_abandoned_call_is_dropped() {
        let mut s = Session::new(Loopback::new(), 0);
        let id = s.begin("echo/slow", None).unwrap();
        s.poll().unwrap();
        s.channel.peer_take();
        s.abandon(id);

        answer_all(&mut s, rmpv::Value::Nil);
        let bytes = frame::encode(&Reply {
            version: PROTOCOL_VERSION,
            id,
            is_final: true,
            body: ReplyBody::Ok {
                value: rmpv::Value::Nil,
            },
        })
        .unwrap();
        s.channel.peer_put(&bytes);

        s.poll().expect("a late answer is not an error");
        assert_eq!(s.take(id).unwrap(), None, "and it is not delivered");
    }

    #[test]
    fn a_reply_to_a_call_never_made_ends_the_session() {
        let mut s = Session::new(Loopback::new(), 0);
        let bytes = frame::encode(&Reply {
            version: PROTOCOL_VERSION,
            id: 99_999,
            is_final: true,
            body: ReplyBody::Ok {
                value: rmpv::Value::Nil,
            },
        })
        .unwrap();
        s.channel.peer_put(&bytes);
        assert!(matches!(
            s.poll(),
            Err(SessionError::Unasked { id: 99_999 })
        ));
    }

    #[test]
    fn a_streamed_reply_refuses_rather_than_half_working() {
        let mut s = Session::new(Loopback::new(), 0);
        let id = s.begin("echo/stream", None).unwrap();
        s.poll().unwrap();
        s.channel.peer_take();

        let bytes = frame::encode(&Reply {
            version: PROTOCOL_VERSION,
            id,
            is_final: false,
            body: ReplyBody::Ok {
                value: rmpv::Value::from(1),
            },
        })
        .unwrap();
        s.channel.peer_put(&bytes);
        assert!(matches!(s.poll(), Err(SessionError::Streamed { .. })));
    }

    #[test]
    fn max_inflight_refuses_the_call_rather_than_queueing_it() {
        let mut s = Session::new(Loopback::new(), 2);
        s.begin("echo/a", None).unwrap();
        s.begin("echo/b", None).unwrap();
        assert!(matches!(
            s.begin("echo/c", None),
            Err(SessionError::Saturated { in_flight: 2 })
        ));
    }

    /// One bad frame desynchronises the stream, so the session must not
    /// look recovered on the next call.
    #[test]
    fn a_failure_is_permanent() {
        let mut s = Session::new(Loopback::new(), 0);
        let mut junk = (frame::MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        junk.extend_from_slice(b"junk");
        s.channel.peer_put(&junk);

        assert!(s.poll().is_err(), "the oversized length is caught");
        assert!(s.poll().is_err(), "and it stays caught");
        assert!(s.begin("echo/say", None).is_err(), "no new calls");
        assert!(s.take(1).is_err(), "and no pending ones complete");
    }

    #[test]
    fn a_dead_channel_reports_through_every_entry_point() {
        let mut s = Session::new(Loopback::new(), 0);
        let id = s.begin("echo/say", None).unwrap();
        s.channel.broken = Some(ChannelError::Closed);

        assert!(s.poll().is_err());
        assert!(s.take(id).is_err());
        assert!(s.begin("echo/again", None).is_err());
    }
}
