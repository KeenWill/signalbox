//! Process-local repository ingestion evidence.

use signalbox_ownership_seam::{OffsetDateTime, RepositorySlug};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// Outcome of the most recently started repository poll.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PollOutcome {
    InProgress,
    Succeeded,
    ClientFailed,
    ObservationFailed,
    StoreFailed,
    FrontierConflict,
    Cancelled,
}

/// One repository poll's start and current outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PollAttempt {
    pub attempted_at: OffsetDateTime,
    pub outcome: PollOutcome,
}

/// Measurements since this store was composed in the current process.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IngestionMeasurements {
    pub last_successful_observation: Option<OffsetDateTime>,
    pub last_poll: Option<PollAttempt>,
    pub last_accepted_webhook: Option<OffsetDateTime>,
    pub events_recorded: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Measurements(Arc<Mutex<BTreeMap<RepositorySlug, IngestionMeasurements>>>);

impl Measurements {
    pub(crate) fn update(
        &self,
        repository: &RepositorySlug,
        update: impl FnOnce(&mut IngestionMeasurements),
    ) {
        let mut measurements = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(measurements.entry(repository.clone()).or_default());
    }
    pub(crate) fn read(&self, repository: &RepositorySlug) -> IngestionMeasurements {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(repository)
            .cloned()
            .unwrap_or_default()
    }
}

pub(crate) struct AttemptGuard {
    measurements: Measurements,
    repository: RepositorySlug,
}
impl AttemptGuard {
    pub(crate) fn start(measurements: Measurements, repository: RepositorySlug) -> Self {
        measurements.update(&repository, |value| {
            value.last_poll = Some(PollAttempt {
                attempted_at: OffsetDateTime::now_utc(),
                outcome: PollOutcome::InProgress,
            })
        });
        Self {
            measurements,
            repository,
        }
    }
    pub(crate) fn finish(self, outcome: PollOutcome) {
        self.measurements.update(&self.repository, |value| {
            if let Some(poll) = &mut value.last_poll {
                poll.outcome = outcome;
            }
        });
    }
}
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        self.measurements.update(&self.repository, |value| {
            if let Some(poll) = &mut value.last_poll
                && poll.outcome == PollOutcome::InProgress
            {
                poll.outcome = PollOutcome::Cancelled;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_attempt_keeps_its_start_and_does_not_replace_successful_observation() {
        let measurements = Measurements::default();
        let repository =
            RepositorySlug::try_new("measurement/project".to_owned()).expect("repository");
        let successful = OffsetDateTime::UNIX_EPOCH;
        measurements.update(&repository, |value| {
            value.last_successful_observation = Some(successful)
        });
        let attempt = AttemptGuard::start(measurements.clone(), repository.clone());
        let started = measurements
            .read(&repository)
            .last_poll
            .expect("running poll");
        assert_eq!(started.outcome, PollOutcome::InProgress);
        drop(attempt);
        let cancelled = measurements.read(&repository);
        assert_eq!(
            cancelled.last_poll,
            Some(PollAttempt {
                attempted_at: started.attempted_at,
                outcome: PollOutcome::Cancelled
            })
        );
        assert_eq!(cancelled.last_successful_observation, Some(successful));
        AttemptGuard::start(measurements.clone(), repository.clone())
            .finish(PollOutcome::ClientFailed);
        assert_eq!(
            measurements
                .read(&repository)
                .last_poll
                .expect("failed poll")
                .outcome,
            PollOutcome::ClientFailed
        );
    }
}
