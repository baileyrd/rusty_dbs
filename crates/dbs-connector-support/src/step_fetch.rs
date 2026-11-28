//! Lazy, step-at-a-time `Connector::fetch` iterators.
//!
//! A connector's `fetch` returns an iterator of events, and the host
//! commits at every `Checkpoint` it reads. If `fetch` builds every event
//! into a `Vec` first, a large first backup does all of its network work
//! (and any per-item downloads) before the host sees a single item: the
//! UI shows "0 fetched" for minutes and nothing is saved until the end.
//!
//! [`step_fetch`] turns a connector's paging loop into a lazy iterator.
//! The connector supplies a *step* closure that does one unit of work
//! (fetch one page, process one feed, download one course...) and queues
//! that unit's events, ending with a checkpoint. The next step runs only
//! once everything queued so far has been consumed, so each unit reaches
//! the host, and is committed, before the next one starts.

use std::collections::VecDeque;

use dbs_core::{ConnectorError, FetchEvent};

/// Events queued by one step, handed out in order.
pub type EventQueue = VecDeque<Result<FetchEvent, ConnectorError>>;

/// What a step tells the driver after queueing its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// More work remains; call the step again once the queue drains.
    More,
    /// The run is finished (or failed). The step is not called again.
    Done,
}

/// Builds a lazy fetch iterator from `step`.
///
/// `step` is called only when every event from earlier steps has been
/// consumed. To fail the run, queue the error and return [`Step::Done`]:
/// events queued before the error still come out first, so pages already
/// fetched are committed, and nothing after it is fetched.
///
/// A step that queues nothing and returns [`Step::More`] is simply called
/// again, which lets a step skip an empty unit (for example, a feed with
/// no new episodes) without special-casing it.
pub fn step_fetch<'a>(
    step: impl FnMut(&mut EventQueue) -> Step + 'a,
) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
    Box::new(StepFetch {
        step: Some(Box::new(step)),
        pending: VecDeque::new(),
    })
}

/// A boxed step closure, as stored by [`StepFetch`].
type StepFn<'a> = Box<dyn FnMut(&mut EventQueue) -> Step + 'a>;

/// The iterator behind [`step_fetch`].
struct StepFetch<'a> {
    /// `None` once a step has returned [`Step::Done`].
    step: Option<StepFn<'a>>,
    pending: EventQueue,
}

impl Iterator for StepFetch<'_> {
    type Item = Result<FetchEvent, ConnectorError>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.pending.is_empty() {
            let step = self.step.as_mut()?;
            if step(&mut self.pending) == Step::Done {
                self.step = None;
            }
        }
        self.pending.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::{Checkpoint, Cursor};
    use std::cell::Cell;

    fn checkpoint(note: &str) -> Result<FetchEvent, ConnectorError> {
        Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: serde_json::json!({}),
            },
            note: note.to_string(),
        }))
    }

    fn note(ev: &Result<FetchEvent, ConnectorError>) -> String {
        match ev {
            Ok(FetchEvent::Checkpoint(cp)) => cp.note.clone(),
            other => panic!("expected a checkpoint, got {other:?}"),
        }
    }

    #[test]
    fn each_step_runs_only_after_the_previous_steps_events_are_consumed() {
        let calls = Cell::new(0);
        let mut iter = step_fetch(|q| {
            calls.set(calls.get() + 1);
            q.push_back(checkpoint(&format!("page {}", calls.get())));
            if calls.get() < 3 {
                Step::More
            } else {
                Step::Done
            }
        });

        assert_eq!(calls.get(), 0, "nothing runs until the first next()");
        assert_eq!(note(&iter.next().unwrap()), "page 1");
        assert_eq!(
            calls.get(),
            1,
            "page 2 isn't fetched while page 1 is in hand"
        );
        assert_eq!(note(&iter.next().unwrap()), "page 2");
        assert_eq!(note(&iter.next().unwrap()), "page 3");
        assert!(iter.next().is_none());
        assert!(iter.next().is_none(), "stays exhausted");
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn an_error_stops_the_run_after_the_events_queued_before_it() {
        let calls = Cell::new(0);
        let events: Vec<_> = step_fetch(|q| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                q.push_back(checkpoint("page 1"));
                return Step::More;
            }
            q.push_back(Err(ConnectorError::Transient("boom".into())));
            Step::Done
        })
        .collect();

        assert_eq!(events.len(), 2);
        assert_eq!(note(&events[0]), "page 1");
        assert!(events[1].is_err());
        assert_eq!(calls.get(), 2, "no step runs after the failing one");
    }

    #[test]
    fn a_step_that_queues_nothing_is_called_again() {
        let calls = Cell::new(0);
        let events: Vec<_> = step_fetch(|q| {
            calls.set(calls.get() + 1);
            match calls.get() {
                1 | 2 => Step::More, // e.g. feeds with nothing new
                _ => {
                    q.push_back(checkpoint("last"));
                    Step::Done
                }
            }
        })
        .collect();

        assert_eq!(events.len(), 1);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn done_with_nothing_queued_ends_immediately() {
        let mut iter = step_fetch(|_| Step::Done);
        assert!(iter.next().is_none());
    }
}
