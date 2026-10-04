//! The providers as TabBeam serves them.
//!
//! The adapters and the runtime [`Provider`] contract they implement live in
//! `seatline-providers`, and know nothing of conversations. This module is
//! TabBeam's side: the [`ConversationProvider`] the request loop drives (a
//! conversation the extension names, with its history and browser context),
//! the registry of what an installed host serves, and the `fake` scaffold.
//! [`crate::conversations::Conversations`] implements the first over the
//! second.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::conversation::{BrowserContext, HistoryMessage};
use crate::conversations::{Conversations, Durability, SessionStore};
use seatline_core::protocol::Capabilities;
pub use seatline_core::stream::BUSY_LIMIT;
use seatline_core::turn::SessionPolicy;

pub mod fake;

pub use seatline_providers::{Cleanup, INVALID_TURN, Provider, claude, codex, gemini, grok};

pub use seatline_platform::layout::Layout;

/// One `conversation.send`, in provider-neutral terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    pub text: String,
    /// Previous user and assistant messages, in order. Adapters without a
    /// native continuation can use these to reconstruct the dialogue.
    pub history: Vec<HistoryMessage>,
    /// The conversation to continue, or `None` to start one.
    pub conversation_id: Option<String>,
    /// Browser context explicitly attached to this turn, after validation
    /// at the native trust boundary.
    pub context: Option<BrowserContext>,
    /// The model to answer with, already a valid model ID; `None` for the
    /// provider's default. Only sent to adapters with `model_selection`.
    pub model: Option<String>,
    /// Whether the selected AI provider should perform its own authenticated
    /// native web search during this same turn.
    pub native_search: bool,
    /// Whether provider-native state may outlive this turn.
    pub session_policy: SessionPolicy,
    /// Start a new native session even when this TabBeam conversation already
    /// has one. The host uses this for search isolation.
    pub fresh_session: bool,
    /// Where the conversation layer tells the host which conversation this
    /// request serves, and whether it just created it.
    pub conversation: ConversationSlot,
}

/// The conversation a request serves. The host creates one per request and
/// reads it when the response starts; the layer that owns conversation IDs
/// fills it in. It replaces the conversation events providers used to send,
/// which are TabBeam's protocol vocabulary, not the runtime's.
#[derive(Debug, Clone, Default)]
pub struct ConversationSlot(Rc<RefCell<SlotState>>);

#[derive(Debug, Default)]
struct SlotState {
    id: Option<String>,
    created: bool,
}

impl ConversationSlot {
    /// The conversation the request serves, once known.
    pub fn id(&self) -> Option<String> {
        self.0.borrow().id.clone()
    }

    /// Whether the request created that conversation, so the host announces
    /// it before the response starts.
    pub fn created(&self) -> bool {
        self.0.borrow().created
    }

    pub fn set(&self, id: impl Into<String>, created: bool) {
        *self.0.borrow_mut() = SlotState {
            id: Some(id.into()),
            created,
        };
    }
}

impl PartialEq for ConversationSlot {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ConversationSlot {}

pub use seatline_core::exchange::{Exchange, Scripted, Timeouts, Update};

/// A provider as the host serves it: a conversation the extension names, with
/// its history and browser context, over a runtime [`Provider`].
pub trait ConversationProvider {
    /// The provider ID requests name, such as `codex`.
    fn id(&self) -> &str;

    fn timeouts(&self) -> Timeouts;

    /// Capabilities that are stable for this adapter implementation. The host
    /// uses these to reject requests that would otherwise be silently degraded.
    fn capabilities(&self) -> Capabilities;

    /// Whether this execution mode supports a provider-native persistent
    /// session that can be resumed by an opaque handle.
    fn supports_persistent_session(&self) -> bool {
        false
    }

    /// Starts checking availability, authentication, and capabilities. The
    /// exchange reports one `Status` and then `Completed`.
    fn status(&self) -> Box<dyn Exchange>;

    /// Starts serving `request`.
    fn send(&self, request: SendRequest) -> Box<dyn Exchange>;

    /// Removes what this adapter keeps for a deleted conversation
    /// (`conversation.forget`): its native-session mapping and, where the
    /// adapter can prove the provider wrote it for TabBeam, the provider's own
    /// transcript. A conversation it doesn't know completes: there is nothing
    /// to remove. An adapter that keeps nothing uses this default.
    fn forget(&self, conversation_id: &str) -> Box<dyn Exchange> {
        let _ = conversation_id;
        Box::new(Scripted::new([Update::Completed]))
    }
}

/// The application name TabBeam's grant in the shared companion is under.
#[cfg(feature = "shared-companion")]
const APP: &str = "tabbeam";

/// The host's connection to the shared companion: one runtime thread and one
/// authenticated connection for the whole host, over which every provider's
/// requests travel side by side however many are in flight. It connects when
/// the first request needs it. A host built without the shared companion runs
/// the adapters itself and has no connection.
struct Link {
    #[cfg(feature = "shared-companion")]
    client: seatline_companion::remote::RemoteClient,
}

impl Link {
    fn new() -> Self {
        Self {
            #[cfg(feature = "shared-companion")]
            client: seatline_companion::remote::RemoteClient::new(APP),
        }
    }
}

#[cfg(feature = "shared-companion")]
fn installed_provider(
    link: &Link,
    metadata: impl Provider,
) -> seatline_companion::client::RemoteProvider {
    seatline_companion::client::RemoteProvider::with_client(APP, link.client.clone(), &metadata)
}

#[cfg(not(feature = "shared-companion"))]
fn installed_provider<P: Provider>(_: &Link, provider: P) -> P {
    provider
}

/// The providers a host serves, in the order `provider.status` reports them.
pub struct Providers(Vec<Box<dyn ConversationProvider>>);

impl Providers {
    pub fn new(providers: Vec<Box<dyn ConversationProvider>>) -> Self {
        Self(providers)
    }

    /// The providers of an installed host. The fake scaffold stays registered
    /// for deterministic protocol diagnostics; real adapters use the same
    /// platform discovery rules.
    pub fn installed(layout: &Layout) -> Self {
        let data = layout.data_dir();
        let link = Link::new();
        let sessions = |name: &str| SessionStore::new(data.as_ref().map(|dir| dir.join(name)));
        Self(vec![
            Box::new(fake::Fake),
            // Codex refuses to start a conversation it couldn't resume after a
            // restart; Claude keeps one in memory when there is no directory.
            Box::new(Conversations::new(
                installed_provider(&link, codex::Codex::installed(layout)),
                sessions("codex-sessions").with_durability(Durability::Required),
            )),
            Box::new(Conversations::new(
                installed_provider(&link, claude::Claude::installed(layout)),
                sessions("claude-sessions"),
            )),
            Box::new(Conversations::new(
                installed_provider(&link, gemini::Gemini::installed(layout)),
                SessionStore::new(None),
            )),
            Box::new(Conversations::new(
                installed_provider(&link, grok::Grok::installed(layout)),
                SessionStore::new(None),
            )),
        ])
    }

    /// Only the deterministic fake scaffold, which starts no processes: for
    /// fuzzing and protocol tests.
    pub fn scaffold() -> Self {
        Self(vec![Box::new(fake::Fake)])
    }

    pub fn get(&self, id: &str) -> Option<&dyn ConversationProvider> {
        self.0
            .iter()
            .map(Box::as_ref)
            .find(|provider| provider.id() == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn ConversationProvider> {
        self.0.iter().map(Box::as_ref)
    }
}

/// Reports every provider's status in turn, then completes: the exchange
/// behind `provider.status` without a provider ID.
pub struct StatusOfAll {
    pending: VecDeque<Box<dyn Exchange>>,
    current: Option<Box<dyn Exchange>>,
}

impl StatusOfAll {
    pub fn new(providers: &Providers) -> Self {
        Self {
            pending: providers.iter().map(ConversationProvider::status).collect(),
            current: None,
        }
    }
}

impl Exchange for StatusOfAll {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        loop {
            let current = match &mut self.current {
                Some(current) => current,
                None => match self.pending.pop_front() {
                    Some(next) => self.current.insert(next),
                    None => return Some(Update::Completed),
                },
            };
            match current.next(deadline)? {
                Update::Completed => self.current = None,
                update => return Some(update),
            }
        }
    }

    fn cancel(&mut self, grace: Duration) {
        self.pending.clear();
        match &mut self.current {
            Some(current) => current.cancel(grace),
            None => self.current = Some(Box::new(Scripted::new([Update::Stopped]))),
        }
    }
}
