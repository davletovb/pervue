//! Protocol-v1 event emission (`docs/protocol/v1.md` §3, §6, §8) and the
//! normalized error and provider-status vocabulary
//! (`docs/protocol/errors-and-capabilities-v1.md`).

use std::io::Write;

use serde::Serialize;

use super::PROTOCOL_VERSION;
use super::request::{FailureKind, RequestFailure, RequestId};
use crate::HOST_VERSION;
use crate::framing::{self, FrameError};

/// v1 event names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    HostReady,
    ProviderStatus,
    ConversationCreated,
    ResponseStarted,
    ResponseDelta,
    ResponseSource,
    ResponseCompleted,
    ResponseFailed,
    RequestCancelled,
}

impl Event {
    pub const fn name(self) -> &'static str {
        match self {
            Self::HostReady => "host.ready",
            Self::ProviderStatus => "provider.status",
            Self::ConversationCreated => "conversation.created",
            Self::ResponseStarted => "response.started",
            Self::ResponseDelta => "response.delta",
            Self::ResponseSource => "response.source",
            Self::ResponseCompleted => "response.completed",
            Self::ResponseFailed => "response.failed",
            Self::RequestCancelled => "request.cancelled",
        }
    }
}

/// Why an event could not be written.
#[derive(Debug)]
pub enum EventError {
    Encode(serde_json::Error),
    Frame(FrameError),
}

pub use seatline_core::protocol::{Authentication, Availability, Capability, ModelOption, Source};

/// The v1 error categories (`errors-and-capabilities-v1.md` §2). The runtime
/// reports a smaller set of its own ([`seatline_core::protocol::ErrorCode`]);
/// the host adds the categories that belong to the browser and the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    HostNotInstalled,
    HostUnavailable,
    ProviderNotFound,
    ProviderNotAuthenticated,
    ProviderFailed,
    SearchFailed,
    RequestCancelled,
    RequestTimeout,
    ContextUnavailable,
    InvalidRequest,
    InternalError,
}

impl From<seatline_core::protocol::ErrorCode> for ErrorCode {
    fn from(code: seatline_core::protocol::ErrorCode) -> Self {
        use seatline_core::protocol::ErrorCode as Runtime;
        match code {
            Runtime::ProviderNotFound => Self::ProviderNotFound,
            Runtime::ProviderNotAuthenticated => Self::ProviderNotAuthenticated,
            Runtime::ProviderFailed => Self::ProviderFailed,
            Runtime::SearchFailed => Self::SearchFailed,
            Runtime::InvalidRequest => Self::InvalidRequest,
            Runtime::InternalError => Self::InternalError,
            // A category a newer runtime added that this host doesn't know:
            // v1 has no better name for it.
            _ => Self::InternalError,
        }
    }
}

/// What the host offers on top of what a provider can do: browser page
/// context and attachments (`errors-and-capabilities-v1.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCapabilities {
    /// Whether the host lets a turn carry page context. It still needs a
    /// provider that isolates tools: page text is untrusted.
    pub page_context: Capability,
    pub attachments: Capability,
}

impl HostCapabilities {
    /// What TabBeam offers: page context wherever the provider can run a
    /// tool-free turn, and no attachments yet.
    pub const TABBEAM: Self = Self {
        page_context: Capability::Supported,
        attachments: Capability::Unsupported,
    };
}

/// Both of two capabilities: unsupported if either is, else unknown if either
/// is, else supported.
const fn both(a: Capability, b: Capability) -> Capability {
    match (a, b) {
        (Capability::Unsupported, _) | (_, Capability::Unsupported) => Capability::Unsupported,
        (Capability::Unknown, _) | (_, Capability::Unknown) => Capability::Unknown,
        (Capability::Supported, Capability::Supported) => Capability::Supported,
    }
}

/// The v1 capability set: the provider's own capabilities plus the host's.
/// Field order is the wire order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub streaming: Capability,
    pub continuation: Capability,
    pub web_search: Capability,
    pub page_context: Capability,
    pub attachments: Capability,
    pub model_selection: Capability,
    pub cancellation: Capability,
}

impl Capabilities {
    pub const fn new(
        provider: seatline_core::protocol::Capabilities,
        host: HostCapabilities,
    ) -> Self {
        Self {
            streaming: provider.streaming,
            continuation: provider.continuation,
            web_search: provider.web_search,
            page_context: both(host.page_context, provider.tool_isolation),
            attachments: host.attachments,
            model_selection: provider.model_selection,
            cancellation: provider.cancellation,
        }
    }
}

/// The v1 `provider.status` state. Account and billing mode
/// ([`seatline_core::protocol::ProviderState::sign_in`]) are deliberately not
/// part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderState {
    pub availability: Availability,
    pub authentication: Authentication,
    pub capabilities: Capabilities,
    /// Suggested models, when `model_selection` is supported. Omitted when
    /// empty.
    #[serde(skip_serializing_if = "<[ModelOption]>::is_empty")]
    pub models: std::borrow::Cow<'static, [ModelOption]>,
}

impl ProviderState {
    pub fn new(state: seatline_core::protocol::ProviderState, host: HostCapabilities) -> Self {
        Self {
            availability: state.availability,
            authentication: state.authentication,
            capabilities: Capabilities::new(state.capabilities, host),
            models: state.models,
        }
    }
}

/// Protocol-v1 error body. Runtime failures are converted to this host-owned
/// shape so provider-runtime crates never carry TabBeam wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ErrorBody<'a> {
    pub code: ErrorCode,
    pub reason: &'a str,
    pub message: &'a str,
    pub retryable: bool,
}

/// Payload of a `provider.status` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus<'a> {
    pub provider_id: &'a str,
    pub status: ProviderState,
}

/// Payload of a `conversation.created` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ConversationCreated<'a> {
    pub conversation_id: &'a str,
}

/// Payload of a `response.started` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResponseStarted<'a> {
    pub provider_id: &'a str,
    /// The conversation being answered, which v1 §6 recommends including.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<&'a str>,
}

/// Payload of a `response.delta` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResponseDelta<'a> {
    pub text: &'a str,
}

/// Payload of a `response.source` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResponseSource<'a> {
    pub source_id: &'a str,
    pub data: &'a Source,
}

/// Payload of a `response.completed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResponseCompleted {}

/// Payload of a `request.cancelled` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RequestCancelled<'a> {
    pub target_request_id: &'a str,
}

#[derive(Serialize)]
struct HostReady {
    host_version: &'static str,
    protocol_versions: [i64; 1],
}

#[derive(Serialize)]
struct ResponseFailed<'a> {
    error: ErrorBody<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<ProtocolVersions>,
}

#[derive(Serialize)]
struct ProtocolVersions {
    received_version: i64,
    supported_versions: [i64; 1],
}

/// Writes the `host.ready` event.
pub fn write_host_ready<W: Write + ?Sized>(output: &mut W) -> Result<(), EventError> {
    let payload = HostReady {
        host_version: HOST_VERSION,
        protocol_versions: [PROTOCOL_VERSION],
    };
    write_envelope(output, None, Event::HostReady, &payload)
}

/// Writes one event for a request. `raw_request_id` is the raw token of a
/// validated [`RequestId`] ([`RequestId::raw`]), which the host keeps after
/// the request's frame is released, so it is echoed byte for byte (v1 §4).
pub fn write_event<W: Write + ?Sized, P: Serialize + ?Sized>(
    output: &mut W,
    raw_request_id: &[u8],
    event: Event,
    payload: &P,
) -> Result<(), EventError> {
    write_envelope(output, Some(raw_request_id), event, payload)
}

/// Writes a `response.failed` event for a request, as [`write_event`] does.
pub fn write_failure<W: Write + ?Sized>(
    output: &mut W,
    raw_request_id: &[u8],
    error: ErrorBody<'_>,
) -> Result<(), EventError> {
    let payload = ResponseFailed {
        error,
        protocol: None,
    };
    write_envelope(
        output,
        Some(raw_request_id),
        Event::ResponseFailed,
        &payload,
    )
}

/// Writes the `response.failed` event for a rejected request (v1 §8).
pub fn write_request_failure<W: Write + ?Sized>(
    output: &mut W,
    failure: &RequestFailure<'_>,
) -> Result<(), EventError> {
    let message = match failure.kind {
        FailureKind::Malformed => "Malformed request.",
        FailureKind::InvalidEnvelope => "Invalid request envelope.",
        FailureKind::InvalidPayload => "Invalid request payload.",
        FailureKind::UnknownMethod => "Unsupported method.",
        FailureKind::UnsupportedVersion => "Unsupported protocol version.",
    };

    // v1 §8.1: a correlated version failure also reports the versions involved.
    let protocol = match (failure.kind, failure.request_id, failure.received_version) {
        (FailureKind::UnsupportedVersion, Some(_), Some(received_version)) => {
            Some(ProtocolVersions {
                received_version,
                supported_versions: [PROTOCOL_VERSION],
            })
        }
        _ => None,
    };

    let payload = ResponseFailed {
        error: ErrorBody {
            code: ErrorCode::InvalidRequest,
            reason: failure.kind.reason(),
            message,
            retryable: false,
        },
        protocol,
    };
    write_envelope(
        output,
        failure.request_id.map(RequestId::raw),
        Event::ResponseFailed,
        &payload,
    )
}

fn write_envelope<W: Write + ?Sized, P: Serialize + ?Sized>(
    output: &mut W,
    raw_request_id: Option<&[u8]>,
    event: Event,
    payload: &P,
) -> Result<(), EventError> {
    let mut frame = Vec::with_capacity(256);
    frame.extend_from_slice(br#"{"version":1,"type":"event","request_id":"#);
    match raw_request_id {
        Some(raw_request_id) => {
            frame.push(b'"');
            frame.extend_from_slice(raw_request_id);
            frame.push(b'"');
        }
        None => frame.extend_from_slice(b"null"),
    }
    frame.extend_from_slice(br#","event":""#);
    frame.extend_from_slice(event.name().as_bytes());
    frame.extend_from_slice(br#"","payload":"#);
    serde_json::to_writer(&mut frame, payload).map_err(EventError::Encode)?;
    frame.push(b'}');
    framing::write_frame(output, &frame).map_err(EventError::Frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::request::parse_request;

    fn frames(mut wire: &[u8]) -> Vec<String> {
        let mut frames = Vec::new();
        while let Some(frame) = framing::read_frame(&mut wire).unwrap() {
            frames.push(String::from_utf8(frame).unwrap());
        }
        frames
    }

    fn runtime_capabilities(tool_isolation: Capability) -> seatline_core::protocol::Capabilities {
        seatline_core::protocol::Capabilities {
            streaming: Capability::Supported,
            continuation: Capability::Supported,
            web_search: Capability::Unsupported,
            model_selection: Capability::Supported,
            cancellation: Capability::Supported,
            tool_isolation,
        }
    }

    #[test]
    fn the_wire_capability_set_keeps_its_v1_order() {
        let wire = serde_json::to_string(&Capabilities::new(
            runtime_capabilities(Capability::Supported),
            HostCapabilities::TABBEAM,
        ))
        .unwrap();
        assert_eq!(
            wire,
            r#"{"streaming":true,"continuation":true,"web_search":false,"page_context":true,"attachments":false,"model_selection":true,"cancellation":true}"#
        );
    }

    #[test]
    fn page_context_needs_the_hosts_offer_and_tool_isolation() {
        let page_context = |host: Capability, isolation: Capability| {
            Capabilities::new(
                runtime_capabilities(isolation),
                HostCapabilities {
                    page_context: host,
                    ..HostCapabilities::TABBEAM
                },
            )
            .page_context
        };
        use Capability::{Supported, Unknown, Unsupported};
        assert_eq!(page_context(Supported, Supported), Supported);
        assert_eq!(page_context(Supported, Unsupported), Unsupported);
        assert_eq!(page_context(Unsupported, Supported), Unsupported);
        assert_eq!(page_context(Supported, Unknown), Unknown);
        assert_eq!(page_context(Unknown, Unsupported), Unsupported);
    }

    #[test]
    fn a_runtime_error_code_keeps_its_v1_name() {
        use seatline_core::protocol::ErrorCode as Runtime;
        for (runtime, wire) in [
            (Runtime::ProviderNotFound, r#""PROVIDER_NOT_FOUND""#),
            (
                Runtime::ProviderNotAuthenticated,
                r#""PROVIDER_NOT_AUTHENTICATED""#,
            ),
            (Runtime::ProviderFailed, r#""PROVIDER_FAILED""#),
            (Runtime::SearchFailed, r#""SEARCH_FAILED""#),
            (Runtime::InvalidRequest, r#""INVALID_REQUEST""#),
            (Runtime::InternalError, r#""INTERNAL_ERROR""#),
        ] {
            assert_eq!(
                serde_json::to_string(&ErrorCode::from(runtime)).unwrap(),
                wire
            );
        }
    }

    #[test]
    fn a_status_never_carries_the_sign_in_classification() {
        let state = ProviderState::new(
            seatline_core::protocol::ProviderState {
                availability: Availability::Available,
                authentication: Authentication::Authenticated,
                capabilities: runtime_capabilities(Capability::Supported),
                models: std::borrow::Cow::Borrowed(&[]),
                sign_in: Some(seatline_core::turn::SignInClassification::ApiKey),
                readiness: Some(seatline_core::readiness::Readiness {
                    source: seatline_core::readiness::Source::Cached,
                    age_ms: 1_200,
                }),
            },
            HostCapabilities::TABBEAM,
        );
        let wire = serde_json::to_string(&state).unwrap();
        assert!(
            !wire.contains("sign_in") && !wire.contains("api_key"),
            "{wire}"
        );
        // Seatline's readiness record is the runtime's, not protocol v1's.
        assert!(
            !wire.contains("readiness") && !wire.contains("age_ms"),
            "{wire}"
        );
        assert!(!wire.contains("models"), "an empty list is omitted: {wire}");
    }

    fn failure_event(input: &str) -> String {
        let failure = parse_request(input.as_bytes()).unwrap_err();
        let mut wire = Vec::new();
        write_request_failure(&mut wire, &failure).unwrap();
        frames(&wire).remove(0)
    }

    #[test]
    fn host_ready_is_uncorrelated() {
        let mut wire = Vec::new();
        write_host_ready(&mut wire).unwrap();
        assert_eq!(
            frames(&wire),
            [format!(
                r#"{{"version":1,"type":"event","request_id":null,"event":"host.ready","payload":{{"host_version":"{HOST_VERSION}","protocol_versions":[1]}}}}"#
            )]
        );
    }

    #[test]
    fn uncorrelated_failures_carry_a_null_request_id() {
        assert_eq!(
            failure_event("{not-json"),
            r#"{"version":1,"type":"event","request_id":null,"event":"response.failed","payload":{"error":{"code":"INVALID_REQUEST","reason":"MALFORMED_MESSAGE","message":"Malformed request.","retryable":false}}}"#
        );
    }

    #[test]
    fn request_ids_are_echoed_byte_for_byte() {
        assert_eq!(
            failure_event(
                r#"{"version":2,"type":"request","request_id":"r\u0065q_v2","method":"provider.status","payload":{}}"#
            ),
            r#"{"version":1,"type":"event","request_id":"r\u0065q_v2","event":"response.failed","payload":{"error":{"code":"INVALID_REQUEST","reason":"UNSUPPORTED_PROTOCOL_VERSION","message":"Unsupported protocol version.","retryable":false},"protocol":{"received_version":2,"supported_versions":[1]}}}"#
        );
    }

    #[test]
    fn every_failure_kind_has_a_stable_reason() {
        for (input, reason) in [
            (
                r#"{"version":1,"type":"request","request_id":"a","method":"x","payload":{},"y":1}"#,
                "INVALID_ENVELOPE",
            ),
            (
                r#"{"version":1,"type":"request","request_id":"a","method":"request.cancel","payload":{}}"#,
                "INVALID_PAYLOAD",
            ),
            (
                r#"{"version":1,"type":"request","request_id":"a","method":"x","payload":{}}"#,
                "UNKNOWN_METHOD",
            ),
        ] {
            let event = failure_event(input);
            assert!(event.contains(r#""request_id":"a""#), "{event}");
            assert!(
                event.contains(&format!(r#""reason":"{reason}""#)),
                "{event}"
            );
        }
    }

    #[test]
    fn suggested_models_are_listed_only_when_there_are_some() {
        const MODELS: &[ModelOption] = &[ModelOption {
            id: std::borrow::Cow::Borrowed("sonnet"),
            label: std::borrow::Cow::Borrowed("Sonnet (latest)"),
        }];
        let mut state = crate::providers::fake::STATUS;
        let without =
            serde_json::to_value(ProviderState::new(state.clone(), HostCapabilities::TABBEAM))
                .unwrap();
        assert!(without.get("models").is_none());
        state.models = std::borrow::Cow::Borrowed(MODELS);
        let with =
            serde_json::to_value(ProviderState::new(state, HostCapabilities::TABBEAM)).unwrap();
        assert_eq!(
            with["models"],
            serde_json::json!([{"id": "sonnet", "label": "Sonnet (latest)"}])
        );
    }

    #[test]
    fn provider_status_uses_the_normalized_vocabulary() {
        let request = parse_request(
            br#"{"version":1,"type":"request","request_id":"req","method":"provider.status","payload":{}}"#,
        )
        .unwrap();
        let status = ProviderStatus {
            provider_id: "codex",
            status: ProviderState {
                availability: Availability::NotFound,
                authentication: Authentication::Unknown,
                capabilities: Capabilities {
                    streaming: Capability::Supported,
                    continuation: Capability::Unsupported,
                    web_search: Capability::Unknown,
                    page_context: Capability::Supported,
                    attachments: Capability::Unsupported,
                    model_selection: Capability::Unknown,
                    cancellation: Capability::Supported,
                },
                models: std::borrow::Cow::Borrowed(&[]),
            },
        };

        let mut wire = Vec::new();
        write_event(
            &mut wire,
            request.request_id.raw(),
            Event::ProviderStatus,
            &status,
        )
        .unwrap();
        assert_eq!(
            frames(&wire),
            [
                r#"{"version":1,"type":"event","request_id":"req","event":"provider.status","payload":{"provider_id":"codex","status":{"availability":"not_found","authentication":"unknown","capabilities":{"streaming":true,"continuation":false,"web_search":"unknown","page_context":true,"attachments":false,"model_selection":"unknown","cancellation":true}}}}"#
            ]
        );
    }

    #[test]
    fn oversized_events_are_not_written() {
        let request = parse_request(
            br#"{"version":1,"type":"request","request_id":"req","method":"provider.status","payload":{}}"#,
        )
        .unwrap();
        let text = "x".repeat(crate::limits::MAX_FRAME_SIZE);
        let mut wire = Vec::new();

        let result = write_event(
            &mut wire,
            request.request_id.raw(),
            Event::ResponseDelta,
            &ResponseDelta { text: &text },
        );
        assert!(matches!(
            result,
            Err(EventError::Frame(FrameError::TooLarge))
        ));
        assert!(wire.is_empty());
    }
}
