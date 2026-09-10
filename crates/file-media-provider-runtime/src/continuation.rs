use std::{collections::BTreeMap, sync::Arc};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{hmac, rand::SystemRandom};
use serde::{Deserialize, Serialize};
use signalbox_file_media_runtime::{
    FileMediaFailure, FileReadInput, FileUse, ReadContinuationCursor, ReadViewName, ReaderIdentity,
    VisiblePartSelector,
};

/// Process-lifetime authentication for restart-ephemeral continuation state.
#[derive(Clone, Debug)]
pub struct ContinuationAuthority(Arc<hmac::Key>);

impl ContinuationAuthority {
    /// Generates one key shared by the composed daemon file tools.
    pub fn generate() -> Result<Self, FileMediaFailure> {
        hmac::Key::generate(hmac::HMAC_SHA256, &SystemRandom::new())
            .map(|key| Self(Arc::new(key)))
            .map_err(|_| FileMediaFailure::ProcessorFailed)
    }

    pub(super) fn seal(
        &self,
        state: &ContinuationState,
    ) -> Result<ReadContinuationCursor, FileMediaFailure> {
        let bytes = serde_json::to_vec(state).map_err(|_| FileMediaFailure::ProcessorFailed)?;
        let tag = hmac::sign(&self.0, &bytes);
        ReadContinuationCursor::try_new(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&bytes),
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        ))
        .map_err(|_| FileMediaFailure::OutputUnitTooLarge)
    }

    pub(super) fn open(
        &self,
        cursor: &ReadContinuationCursor,
    ) -> Result<ContinuationState, FileMediaFailure> {
        let invalid = || FileMediaFailure::InvalidViewArguments;
        let (bytes, tag) = cursor.as_str().split_once('.').ok_or_else(invalid)?;
        let bytes = URL_SAFE_NO_PAD.decode(bytes).map_err(|_| invalid())?;
        let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| invalid())?;
        hmac::verify(&self.0, &bytes, &tag).map_err(|_| invalid())?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct ContinuationState {
    digest: String,
    selector: String,
    view: String,
    provider: String,
    reader: String,
    revision: String,
    pub(super) options: BTreeMap<String, serde_json::Value>,
    cursor: String,
}

impl ContinuationState {
    pub(super) fn new(
        source: &FileUse,
        selector: &VisiblePartSelector,
        view: &ReadViewName,
        reader: &ReaderIdentity,
        options: BTreeMap<String, serde_json::Value>,
        cursor: &ReadContinuationCursor,
    ) -> Self {
        Self {
            digest: source.digest().to_string(),
            selector: selector.as_str().to_owned(),
            view: view.as_str().to_owned(),
            provider: reader.provider().as_str().to_owned(),
            reader: reader.reader().as_str().to_owned(),
            revision: reader.revision().as_str().to_owned(),
            options,
            cursor: cursor.as_str().to_owned(),
        }
    }

    pub(super) fn matches(
        &self,
        source: &FileUse,
        selector: &VisiblePartSelector,
        view: &ReadViewName,
    ) -> bool {
        self.digest == source.digest().to_string()
            && self.selector == selector.as_str()
            && self.view == view.as_str()
    }

    pub(super) fn reader(&self) -> Result<ReaderIdentity, FileMediaFailure> {
        use signalbox_file_media_runtime::{
            FileReaderName, FileReaderProviderName, FileReaderRevision,
        };
        let invalid = |_| FileMediaFailure::InvalidViewArguments;
        Ok(ReaderIdentity::new(
            FileReaderProviderName::try_new(self.provider.clone()).map_err(invalid)?,
            FileReaderName::try_new(self.reader.clone()).map_err(invalid)?,
            FileReaderRevision::try_new(self.revision.clone()).map_err(invalid)?,
        ))
    }

    pub(super) fn runtime_input(&self) -> Result<FileReadInput, FileMediaFailure> {
        Ok(FileReadInput::Continuation {
            cursor: ReadContinuationCursor::try_new(self.cursor.clone())
                .map_err(|_| FileMediaFailure::InvalidViewArguments)?,
        })
    }
}
