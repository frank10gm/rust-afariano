//! `/api/v1/runner/*` (see `rust-afariano-proto` for the contract) and the small
//! pipeline API for scripts (`/api/v1/repos/{ns}/{name}/pipelines`).

use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_afariano_proto as proto;
use serde::Deserialize;

use crate::app::Shared;
use crate::auth::{self, ApiCaller, bearer};
use crate::error::{AppError, AppResult, bad};
use crate::logs::Append;
use crate::models::{PipelineVar, Runner, User};
use crate::pipelines::{self, Created, NewPipeline};

async fn runner_auth(state: &Shared, headers: &HeaderMap) -> AppResult<Runner> {
    let token = bearer(headers).ok_or(AppError::Unauthorized)?;
    auth::runner_by_token(state, token).await?.ok_or(AppError::Unauthorized)
}

async fn record_hello(state: &Shared, runner: &Runner, hello: &proto::RunnerHello) -> AppResult<()> {
    let clip = |s: &str| s.chars().take(100).collect::<String>();
    sqlx::query("UPDATE runners SET version = $2, platform = $3, executor = $4, hostname = $5 WHERE id = $1")
        .bind(runner.id)
        .bind(clip(&hello.version))
        .bind(clip(&hello.platform))
        .bind(clip(&hello.executor))
        .bind(clip(&hello.hostname))
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn verify(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(hello): Json<proto::RunnerHello>,
) -> AppResult<Json<proto::RunnerInfo>> {
    let runner = runner_auth(&state, &headers).await?;
    record_hello(&state, &runner, &hello).await?;
    tracing::info!(runner = %runner.name, version = %hello.version, platform = %hello.platform, "runner verified");
    Ok(Json(proto::RunnerInfo {
        id: runner.id,
        name: runner.name,
        tags: runner.tags,
        run_untagged: runner.run_untagged,
    }))
}

pub async fn request_job(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(hello): Json<proto::RunnerHello>,
) -> AppResult<Response> {
    let runner = runner_auth(&state, &headers).await?;
    if runner.version != hello.version || runner.executor != hello.executor || runner.platform != hello.platform {
        record_hello(&state, &runner, &hello).await?;
    }
    if runner.paused {
        tokio::time::sleep(Duration::from_secs(proto::REQUEST_LONG_POLL_SECS)).await;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(proto::REQUEST_LONG_POLL_SECS);
    loop {
        // Register interest before looking, so a job queued in between still wakes us.
        let notified = state.jobs_ready.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(spec) = pipelines::assign(&state, &runner).await? {
            return Ok(Json(spec).into_response());
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }
}

#[derive(Deserialize)]
pub struct LogQuery {
    offset: u64,
}

pub async fn put_log(
    State(state): State<Shared>,
    Path(job_id): Path<i64>,
    Query(q): Query<LogQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    let token = bearer(&headers).ok_or(AppError::Unauthorized)?;
    let job = auth::job_by_token(&state, token).await?.ok_or(AppError::Forbidden)?;
    if job.id != job_id {
        return Err(AppError::Forbidden);
    }
    if body.len() > proto::MAX_LOG_CHUNK {
        return Err(bad("log chunk too large"));
    }
    let result = state.logs.append(job_id, q.offset, &body, proto::MAX_LOG_SIZE).await?;
    let (status, size) = match result {
        Append::Ok(size) => (StatusCode::OK, size),
        Append::Mismatch(size) => (StatusCode::CONFLICT, size),
    };
    // Every PUT is also the heartbeat the reaper looks at.
    sqlx::query("UPDATE jobs SET log_size = $2, updated_at = now() WHERE id = $1")
        .bind(job_id)
        .bind(size as i64)
        .execute(&state.db)
        .await?;
    Ok((status, Json(proto::LogAck { size, cancel: job.cancel_requested })).into_response())
}

pub async fn finish_job(
    State(state): State<Shared>,
    Path(job_id): Path<i64>,
    headers: HeaderMap,
    Json(result): Json<proto::JobResult>,
) -> AppResult<StatusCode> {
    let token = bearer(&headers).ok_or(AppError::Unauthorized)?;
    let job = auth::job_by_token(&state, token).await?.ok_or(AppError::Forbidden)?;
    if job.id != job_id {
        return Err(AppError::Forbidden);
    }
    let status = match result.outcome {
        proto::JobOutcome::Success => "success",
        proto::JobOutcome::Failed => "failed",
        proto::JobOutcome::Canceled => "canceled",
    };
    pipelines::finish(&state, job_id, status, result.exit_code, result.failure_reason.map(|r| r.as_str())).await?;
    tracing::info!(job = job_id, status, "job finished");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TriggerBody {
    #[serde(rename = "ref")]
    ref_name: String,
    #[serde(default)]
    variables: Vec<PipelineVar>,
}

/// `POST /api/v1/repos/{ns}/{name}/pipelines` with `Authorization: Bearer rfp_…`
/// and `{"ref": "main", "variables": [{"key": "X", "value": "1"}]}`.
///
/// A running job may call it with its `CI_JOB_TOKEN` instead: the new pipeline then
/// remembers the job ("Started by release:gate in #9"). Such a job starts pipelines
/// only on its own ref, as the account that started its pipeline, so it reaches no
/// variables its own pipeline could not; and a pipeline started this way starts no
/// more, so jobs cannot loop.
pub async fn trigger_pipeline(
    State(state): State<Shared>,
    Path((ns, name)): Path<(String, String)>,
    caller: ApiCaller,
    Json(body): Json<TriggerBody>,
) -> AppResult<Response> {
    let (repo, user, parent) = match caller {
        ApiCaller::User(user) => {
            let (repo, _) = crate::access::repo(&state, &user, &ns, &name, crate::access::Role::Write).await?;
            (repo, Some(user), None)
        }
        ApiCaller::Job(job) => {
            let repo = crate::access::job_repo(&state, &job, &ns, &name).await?;
            let up = pipelines::get_pipeline(&state, job.pipeline_id).await?;
            if up.parent_job_id.is_some() {
                return Err(bad("a pipeline started by a job cannot start more pipelines"));
            }
            if body.ref_name != up.ref_name {
                return Err(bad(format!("a job token starts pipelines only on its own ref `{}`", up.ref_name)));
            }
            let user = match up.created_by {
                Some(id) => {
                    sqlx::query_as::<_, User>("SELECT id, email, name FROM users WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&state.db)
                        .await?
                }
                None => None,
            };
            (repo, user, Some((job, up)))
        }
    };
    if repo.is_mirror()
        && let Err(e) = crate::mirror::sync_for_pipeline(&state, &repo).await
    {
        return Err(crate::error::AppError::BadGateway(format!(
            "no pipeline started: could not fetch the upstream. {e}"
        )));
    }
    let git = state.git(repo.id);
    let (full, is_tag) = if git.resolve(&format!("refs/heads/{}", body.ref_name)).await?.is_some() {
        (format!("refs/heads/{}", body.ref_name), false)
    } else if git.resolve(&format!("refs/tags/{}", body.ref_name)).await?.is_some() {
        (format!("refs/tags/{}", body.ref_name), true)
    } else {
        return Err(bad(format!("unknown branch or tag `{}`", body.ref_name)));
    };
    let sha = git.resolve(&full).await?.ok_or_else(|| bad("ref vanished"))?;
    if let Some((_, up)) = &parent
        && up.ref_is_tag != is_tag
    {
        return Err(bad(format!("a job token starts pipelines only on its own ref `{}`", up.ref_name)));
    }
    let np = NewPipeline {
        repo: &repo,
        ref_name: body.ref_name.clone(),
        is_tag,
        sha,
        before_sha: crate::git::ZERO_SHA.into(),
        source: "api",
        user: user.as_ref(),
        variables: crate::web::clean_vars(body.variables)?,
        parent_job_id: parent.as_ref().map(|(job, _)| job.id),
    };
    let created = pipelines::create(&state, np).await?;
    if let (Created::Pipeline(_), Some((_, up))) = (&created, &parent) {
        // Its page now names the new pipeline ("started #10").
        state.pipeline_changed(repo.id, up.id);
    }
    match created {
        Created::Pipeline(iid) => Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({
                "iid": iid,
                "web_url": format!("{}/{}/-/pipelines/{iid}", state.cfg.public_url, repo.path()),
            })),
        )
            .into_response()),
        Created::Filtered(reason) => Err(bad(format!("no pipeline: {reason}"))),
        Created::NoConfig => Err(bad(format!("no {} at {}", pipelines::CI_FILE, body.ref_name))),
    }
}
