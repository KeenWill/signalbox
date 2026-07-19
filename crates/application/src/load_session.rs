//! Current-session query orchestration.
//!
//! ADR-0038 defines [`Session`] as the complete current domain snapshot and
//! distinguishes a true missing session from adapter integrity failure. This
//! application boundary loads by semantic [`SessionId`] and leaves durable
//! record decoding and checked reconstitution to an adapter.

use std::future::Future;

use signalbox_domain::{Session, SessionId};

/// Application-owned port for loading one complete current session snapshot.
///
/// `Ok(None)` means only that the requested session does not exist in the
/// adapter's read snapshot. Missing or malformed facts for an existing session
/// are adapter errors rather than absence.
pub trait SessionReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Loads the complete current projection selected by `session_id`.
    fn load_session(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<Option<Session>, Self::Error>> + Send;
}

/// Coordinates the current-session query use case.
#[derive(Debug)]
pub struct LoadSessionService<Reader> {
    reader: Reader,
}

impl<Reader> LoadSessionService<Reader> {
    /// Composes the application query with its current-session reader.
    pub const fn new(reader: Reader) -> Self {
        Self { reader }
    }

    /// Returns the reader, primarily for explicit ownership handoff.
    pub fn into_reader(self) -> Reader {
        self.reader
    }
}

impl<Reader> LoadSessionService<Reader>
where
    Reader: SessionReader,
{
    /// Loads one complete current domain snapshot by semantic identity.
    ///
    /// The application neither retries nor translates absence, success, or
    /// adapter failure. In particular, it does not consult a creation receipt
    /// or reconstruct a `Session` from persistence records.
    pub async fn execute(&self, session_id: SessionId) -> Result<Option<Session>, Reader::Error> {
        self.reader.load_session(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, ready},
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    use signalbox_domain::{
        DirectModelSelection, ModelSelectionRequest, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionReconstitutionInput, TranscriptAncestry,
    };
    use uuid::Uuid;

    use super::{LoadSessionService, Session, SessionId, SessionReader};

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    /// A complete current session snapshot whose current defaults carry the
    /// given version. The model-selection seed derives from that one knob,
    /// decorrelated (`docs/testing-style.md`, rule 4), so a projection reading
    /// one value where it should read the other cannot accidentally pass.
    fn current_session(id: SessionId, defaults_version: u64) -> Session {
        let version = SessionConfigurationDefaultsVersion::try_from_u64(defaults_version)
            .expect("test version is positive");
        let decorrelated_model = u128::from(u64::MAX - defaults_version);
        SessionReconstitutionInput::new(
            id,
            id,
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            id,
            version,
            id,
            version,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::from_u128(decorrelated_model)),
            )),
        )
        .reconstitute()
        .expect("test facts form one complete current session")
    }

    fn run_ready<Output>(future: impl Future<Output = Output>) -> Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("fake-backed use case must be immediately ready"),
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeReaderError {
        Unavailable,
    }

    #[derive(Debug)]
    struct FakeSessionReader {
        observed: Mutex<Vec<SessionId>>,
        result: Result<Option<Session>, FakeReaderError>,
    }

    impl FakeSessionReader {
        fn returning(result: Result<Option<Session>, FakeReaderError>) -> Self {
            Self {
                observed: Mutex::new(Vec::new()),
                result,
            }
        }

        fn observed(self) -> Vec<SessionId> {
            self.observed
                .into_inner()
                .expect("fake reader observation lock is not poisoned")
        }
    }

    impl SessionReader for FakeSessionReader {
        type Error = FakeReaderError;

        fn load_session(
            &self,
            session_id: SessionId,
        ) -> impl Future<Output = Result<Option<Session>, Self::Error>> + Send {
            self.observed
                .lock()
                .expect("fake reader observation lock is not poisoned")
                .push(session_id);
            ready(self.result.clone())
        }
    }

    /// S01 / INV-002 / INV-008: application orchestration returns the exact
    /// complete domain snapshot supplied by the current-session port.
    #[test]
    fn s01_inv002_inv008_complete_current_session_is_returned_unchanged() {
        let requested = session_id(1);
        let current = current_session(requested, 4);
        let service =
            LoadSessionService::new(FakeSessionReader::returning(Ok(Some(current.clone()))));

        let loaded = run_ready(service.execute(requested))
            .expect("fake current-session query succeeds")
            .expect("fake current session exists");

        assert_eq!(loaded, current);
        assert_eq!(
            loaded.current_configuration_defaults().version(),
            current.current_configuration_defaults().version()
        );
        assert_eq!(service.into_reader().observed(), [requested]);
    }

    /// S01 / ADR-0038: true session absence remains `None`; the application
    /// does not fabricate an initial or partial projection.
    #[test]
    fn s01_true_session_absence_is_preserved() {
        let requested = session_id(1);
        let service = LoadSessionService::new(FakeSessionReader::returning(Ok(None)));

        let loaded = run_ready(service.execute(requested)).expect("absence is not an error");

        assert_eq!(loaded, None);
        assert_eq!(service.into_reader().observed(), [requested]);
    }

    /// S01 / INV-012: loading by semantic session identity is a single query;
    /// an adapter failure is returned without retry or command handling.
    #[test]
    fn s01_inv012_reader_failure_is_returned_without_retry() {
        let requested = session_id(1);
        let service = LoadSessionService::new(FakeSessionReader::returning(Err(
            FakeReaderError::Unavailable,
        )));

        let error =
            run_ready(service.execute(requested)).expect_err("adapter failure remains nonterminal");

        assert_eq!(error, FakeReaderError::Unavailable);
        assert_eq!(service.into_reader().observed(), [requested]);
    }
}
