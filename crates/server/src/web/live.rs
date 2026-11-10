//! Live pages: a server-sent event stream pings the pipeline list, a pipeline and a
//! job page when what they show changes; the page then fetches itself and swaps the
//! changed parts in place (`app.js`, `data-live`). The ping carries no data, so the
//! fetch goes through the usual access checks.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use serde::Deserialize;
use tokio::sync::broadcast;

use super::load_repo;
use crate::access::{self, Role};
use crate::app::{PipelineChanged, Shared};
use crate::auth::CurrentUser;
use crate::error::{AppError, AppResult};

/// Value of a page's `data-live` attribute: the stream for it, and the change
/// counter read before its data was loaded (`data-live-seq` in the templates).
pub struct LiveUrl {
    pub url: String,
    pub seq: u64,
}

impl LiveUrl {
    pub fn new(state: &Shared, url: String) -> Self {
        Self { url, seq: state.change_seq() }
    }
}

#[derive(Deserialize)]
pub struct LiveQuery {
    /// Only changes of this pipeline (its number in the repository).
    pipeline: Option<i32>,
    /// The change counter the page was rendered at: a different one now means the
    /// page missed a change, and the stream pings at once.
    #[serde(default)]
    since: Option<u64>,
}

/// Changes of a repository's pipelines, or of one of them.
pub async fn repo_events(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Query(q): Query<LiveQuery>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let pipeline = match q.pipeline {
        Some(iid) => Some(
            sqlx::query_scalar::<_, i64>("SELECT id FROM pipelines WHERE repo_id = $1 AND iid = $2")
                .bind(repo.id)
                .bind(iid)
                .fetch_optional(&state.db)
                .await?
                .ok_or(AppError::NotFound)?,
        ),
        None => None,
    };
    let repo_id = repo.id;
    Ok(sse(stream(&state, q.since, move |c| c.repo_id == repo_id && pipeline.is_none_or(|p| p == c.pipeline_id))))
}

/// Changes in every repository the user can see (the home page).
pub async fn home_events(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<LiveQuery>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    // None: an administrator, who sees every repository.
    let visible = access::visible_repo_ids(&state, &user).await?;
    Ok(sse(stream(&state, q.since, move |c| visible.as_ref().is_none_or(|ids| ids.contains(&c.repo_id)))))
}

/// The ping a page reacts to.
pub fn change_event() -> Event {
    Event::default().event("change").data("{}")
}

fn sse(
    s: impl Stream<Item = Result<Event, Infallible>> + Send + 'static,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn stream<F: Fn(&PipelineChanged) -> bool + Send + 'static>(
    state: &Shared,
    since: Option<u64>,
    keep: F,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + use<F> {
    // Subscribe before reading the counter: a change in between is then either
    // in the channel or in the counter, never in neither.
    let rx = state.pipeline_events.subscribe();
    let behind = since.is_some_and(|s| s != state.change_seq());
    changes(rx, behind, keep)
}

/// One `change` per relevant event (at once if `behind`). Dropped with the response
/// when the client goes away.
fn changes(
    rx: broadcast::Receiver<PipelineChanged>,
    behind: bool,
    keep: impl Fn(&PipelineChanged) -> bool + Send + 'static,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    futures_util::stream::unfold((rx, keep, behind), |(mut rx, keep, behind)| async move {
        if behind {
            return Some((Ok(change_event()), (rx, keep, false)));
        }
        loop {
            match rx.recv().await {
                Ok(c) if !keep(&c) => continue,
                // Missed some: one ping covers them all.
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    return Some((Ok(change_event()), (rx, keep, false)));
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt as _;

    fn changed(repo_id: i64, pipeline_id: i64) -> PipelineChanged {
        PipelineChanged { repo_id, pipeline_id }
    }

    async fn next_within<S: Stream + Unpin>(s: &mut S, ms: u64) -> Option<S::Item> {
        tokio::time::timeout(Duration::from_millis(ms), s.next()).await.ok().flatten()
    }

    #[tokio::test]
    async fn pings_for_kept_changes_only() {
        let (tx, rx) = broadcast::channel(16);
        let mut s = Box::pin(changes(rx, false, |c| c.repo_id == 1));
        assert!(next_within(&mut s, 50).await.is_none(), "nothing changed yet");
        tx.send(changed(2, 20)).unwrap();
        assert!(next_within(&mut s, 50).await.is_none(), "another repository");
        tx.send(changed(1, 10)).unwrap();
        assert!(next_within(&mut s, 50).await.is_some());
        drop(tx);
        assert!(next_within(&mut s, 50).await.is_none(), "ends with the channel");
    }

    #[tokio::test]
    async fn behind_pings_at_once() {
        let (_tx, rx) = broadcast::channel::<PipelineChanged>(16);
        let mut s = Box::pin(changes(rx, true, |_| false));
        assert!(next_within(&mut s, 50).await.is_some());
        assert!(next_within(&mut s, 50).await.is_none(), "only once");
    }

    #[tokio::test]
    async fn lagging_pings_once() {
        let (tx, rx) = broadcast::channel(2);
        let mut s = Box::pin(changes(rx, false, |_| true));
        for i in 0..5 {
            tx.send(changed(1, i)).unwrap();
        }
        // Lagged, then the two events still in the channel.
        let mut n = 0;
        while next_within(&mut s, 50).await.is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
    }
}
