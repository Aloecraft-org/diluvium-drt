//! Addressing a peer, and saying who a message came from.
//!
//! A cross-peer write is a queue write with an identity attached: the node
//! writes to a foreign queue the way it writes to a local one, its runtime
//! signs and delivers, and the receiving node reads a queue and finds the
//! sender its own runtime verified. **No node ever sees a signature**, and
//! nothing in this module signs anything — this is the vocabulary the
//! delivery path speaks, reserved ahead of the slice that builds it.
//!
//! Nothing here reaches a peer in this build. [`QueueAddress`] carries a
//! peer component so the hostcall ABI does not have to change when one
//! does, and a write that sets it is a named failure rather than a stub
//! that half works.
//!
//! ## surface block
//!
//! - Entry points: [`QueueAddress`], where a message is going;
//!   [`Sender`], where one came from; [`QueueAddress::local`], the only
//!   form this build delivers.
//! - Configurable values: [`SENDER_KEY`], the reserved key a delivered
//!   message carries its sender under.
//! - Fan-out: [`PeerRef`] is the two things a peer can be, and the only
//!   place that distinction is made.

use serde::{Deserialize, Serialize};

use crate::id::Uuid7;
use crate::project::NodePath;

/// The key a delivered message carries its sender under.
///
/// Reserved: a program must not use it for anything else, because the
/// runtime writes it. Named `from` rather than `sender` because it reads
/// beside `to` at a call site and because the lifecycle events already use
/// short keys.
pub const SENDER_KEY: &str = "from";

/// What sits on the other end: another drt root, or a plugin.
///
/// **Not typed as a `root_id`.** A plugin has none — it is named by the
/// role its package satisfies — and a type that could only hold a uuid
/// would force the plugin case into a second code path later, which is
/// exactly the rework this round exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PeerRef {
    /// A drt root, by the id it was minted with. Also how a **local**
    /// sender names itself: a node's own root is a root like any other, so
    /// a reader does not need a separate "this one is local" case.
    Root { root_id: Uuid7 },
    /// A plugin, by name. The name is the role its binding satisfies.
    Plugin { name: String },
    /// This drt, running **without a root**: `drt run app.dlua`, where
    /// there is no `.drt_root/` and so no `root_id` to name.
    ///
    /// Not addressable, and it never appears on a wire. It exists because
    /// "every delivered message carries a sender" has to be true on the
    /// no-root path too, and the honest answer there is not a made-up id.
    /// A no-root deployment has no peers by construction — no
    /// `project.json` to declare one, no `consent.json` to bind one — so
    /// nothing is lost by it being unaddressable.
    Runtime,
}

impl PeerRef {
    pub fn root(root_id: Uuid7) -> PeerRef {
        PeerRef::Root { root_id }
    }

    pub fn plugin(name: impl Into<String>) -> PeerRef {
        PeerRef::Plugin { name: name.into() }
    }

    /// Is this the root the runtime is running? The one question a local
    /// delivery path asks.
    pub fn is_root(&self, id: Uuid7) -> bool {
        matches!(self, PeerRef::Root { root_id } if *root_id == id)
    }
}

impl std::fmt::Display for PeerRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerRef::Root { root_id } => write!(f, "root {root_id}"),
            PeerRef::Plugin { name } => write!(f, "plugin '{name}'"),
            PeerRef::Runtime => f.write_str("this runtime, which has no root"),
        }
    }
}

/// Where a message is going: a queue, and optionally a peer to find it on.
///
/// A struct rather than a string so the peer component can arrive without
/// the hostcall ABI changing shape. Every write this build makes leaves it
/// `None`; one that sets it is refused by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueAddress {
    /// `None` is this root. Anything else is not deliverable in this build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<PeerRef>,
    pub queue: String,
}

impl QueueAddress {
    /// A queue in this root — the only address this build delivers to.
    pub fn local(queue: impl Into<String>) -> QueueAddress {
        QueueAddress {
            peer: None,
            queue: queue.into(),
        }
    }

    /// A queue on a peer. Constructible so a caller can be written against
    /// it now; [`QueueAddress::deliverable`] is what refuses it.
    pub fn on(peer: PeerRef, queue: impl Into<String>) -> QueueAddress {
        QueueAddress {
            peer: Some(peer),
            queue: queue.into(),
        }
    }

    /// The named failure a write with a peer component gets.
    ///
    /// One function so that every path refusing peer delivery says the same
    /// sentence — a reader who sees it twice should not have to wonder
    /// whether the two mean different things.
    pub fn deliverable(&self) -> Result<&str, PeerDeliveryUnsupported> {
        match &self.peer {
            None => Ok(&self.queue),
            Some(peer) => Err(PeerDeliveryUnsupported {
                peer: peer.to_string(),
                queue: self.queue.clone(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("peer delivery is not supported in this build: '{queue}' on {peer}")]
pub struct PeerDeliveryUnsupported {
    pub peer: String,
    pub queue: String,
}

/// Who a delivered message came from.
///
/// Always present on a delivered message, local or not. A local sender
/// names its own root, so a node reads one field and never has to ask
/// "was this one of mine?" as a separate question — which is the property
/// that makes a node written today work unchanged when the first message
/// arrives from somewhere else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub peer: PeerRef,
    /// The **local** form, in the sending root. A node path means something
    /// only inside its own root, and the root is right there in `peer`, so
    /// the qualified spelling would carry it twice.
    pub node: NodePath,
}

impl Sender {
    pub fn new(peer: PeerRef, node: NodePath) -> Sender {
        Sender { peer, node }
    }

    /// A sender in this root.
    pub fn local(root_id: Uuid7, node: NodePath) -> Sender {
        Sender {
            peer: PeerRef::root(root_id),
            node,
        }
    }

    /// A sender on the no-root path, where there is no id to name.
    pub fn runtime(node: NodePath) -> Sender {
        Sender {
            peer: PeerRef::Runtime,
            node,
        }
    }

    /// The msgpack value a delivered message carries under [`SENDER_KEY`].
    ///
    /// Hand-built rather than derived so the wire shape is a decision in
    /// code that fails to compile if the type gains a field: a guest reads
    /// these two keys by name, and adding a third silently would be a
    /// change to a format two implementations have to agree on.
    pub fn to_value(&self) -> rmpv::Value {
        let peer = match &self.peer {
            PeerRef::Root { root_id } => vec![
                ("kind".into(), rmpv::Value::from("root")),
                ("root_id".into(), rmpv::Value::from(root_id.to_string())),
            ],
            PeerRef::Plugin { name } => vec![
                ("kind".into(), rmpv::Value::from("plugin")),
                ("name".into(), rmpv::Value::from(name.as_str())),
            ],
            PeerRef::Runtime => vec![("kind".into(), rmpv::Value::from("runtime"))],
        };
        rmpv::Value::Map(vec![
            ("peer".into(), rmpv::Value::Map(peer)),
            ("node".into(), rmpv::Value::from(self.node.as_str())),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Uuid7 {
        Uuid7::mint(1_757_707_440_000, [0x55; 10])
    }

    fn node() -> NodePath {
        NodePath::root().child("intake").unwrap()
    }

    /// Seam 3: every write this build makes is local, and one that is not
    /// is refused by name rather than ignored.
    #[test]
    fn a_local_address_delivers_and_a_peer_one_is_refused_by_name() {
        let local = QueueAddress::local("http_in");
        assert_eq!(local.deliverable().unwrap(), "http_in");

        for peer in [PeerRef::root(id()), PeerRef::plugin("webauthn")] {
            let addressed = QueueAddress::on(peer, "query");
            let e = addressed.deliverable().unwrap_err();
            assert!(
                e.to_string().contains("peer delivery is not supported"),
                "{e}"
            );
            assert!(e.to_string().contains("query"), "it names the queue: {e}");
        }
    }

    /// The peer component is not a `root_id`, so a plugin needs no second
    /// code path when it arrives.
    #[test]
    fn a_plugin_is_addressable_without_having_an_id() {
        let plugin = QueueAddress::on(PeerRef::plugin("webauthn"), "assert");
        let round = serde_json::to_string(&plugin).unwrap();
        assert!(round.contains("\"kind\":\"plugin\""), "{round}");
        assert!(!round.contains("root_id"), "{round}");
        assert_eq!(
            serde_json::from_str::<QueueAddress>(&round).unwrap(),
            plugin
        );
    }

    /// A local address carries no peer key at all, so the ordinary case
    /// costs nothing on the wire.
    #[test]
    fn a_local_address_puts_nothing_on_the_wire_for_the_peer() {
        let json = serde_json::to_string(&QueueAddress::local("http_in")).unwrap();
        assert_eq!(json, r#"{"queue":"http_in"}"#);
    }

    /// Seam 2: a local sender names its own root, so one field answers
    /// "who sent this" whether or not it came from here.
    #[test]
    fn a_local_sender_names_its_own_root() {
        let from = Sender::local(id(), node());
        assert!(from.peer.is_root(id()));
        assert_eq!(from.node, node());

        let rmpv::Value::Map(pairs) = from.to_value() else {
            panic!("a sender is a map");
        };
        let keys: Vec<_> = pairs
            .iter()
            .filter_map(|(k, _)| k.as_str().map(str::to_string))
            .collect();
        assert_eq!(keys, ["peer", "node"]);
    }

    /// The node path in a sender is the local form: the root is in `peer`
    /// already, and spelling it twice is how two implementations end up
    /// disagreeing about which one is authoritative.
    #[test]
    fn the_senders_node_is_the_local_form() {
        let encoded = format!("{:?}", Sender::local(id(), node()).to_value());
        assert!(encoded.contains("root/intake"), "{encoded}");
        assert!(
            !encoded.contains(&format!("{}/root/intake", id())),
            "the qualified form is for addressing, not for saying who sent something: {encoded}"
        );
    }
}
