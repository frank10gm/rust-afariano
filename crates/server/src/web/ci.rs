use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use futures_util::Stream;
use rust_afariano_ci::FormVariable;
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;

use super::{JobRow, PipelineRow, RepoNav, clean_vars, load_repo, render, user_names};
use crate::access::Role;
use crate::app::Shared;
use crate::auth::CurrentUser;
use crate::error::{AppError, AppResult, bad};
use crate::logs::LogEvent;
use crate::models::{
    JOB_COLS, Job, PIPELINE_COLS, Pipeline, PipelineVar, RUNNER_COLS, Repo, Runner, User, is_terminal,
};
use crate::pipelines::{self, CI_FILE, Created, NewPipeline};
use crate::util::ago;

#[derive(Template)]
#[template(path = "pipelines.html")]
struct PipelinesPage {
    user: User,
    nav: RepoNav,
    pipelines: Vec<PipelineRow>,
    show_repo: bool,
    prev_url: String,
    next_url: String,
    refresh: bool,
}

#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    page: i64,
}

pub async fn pipelines(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    const PER_PAGE: i64 = 30;
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let page = q.page.max(0);
    let list: Vec<Pipeline> = sqlx::query_as(sql!(
        "SELECT {PIPELINE_COLS} FROM pipelines WHERE repo_id = $1 ORDER BY id DESC LIMIT $2 OFFSET $3"
    ))
    .bind(repo.id)
    .bind(PER_PAGE + 1)
    .bind(page * PER_PAGE)
    .fetch_all(&state.db)
    .await?;
    let names = user_names(&state, list.iter().filter_map(|p| p.created_by)).await?;
    let rp = repo.path();
    let has_next = list.len() as i64 > PER_PAGE;
    let mut rows: Vec<PipelineRow> = list
        .iter()
        .take(PER_PAGE as usize)
        .map(|p| {
            PipelineRow::new(p, &rp, p.created_by.and_then(|id| names.get(&id)).map(String::as_str)).for_repo(&repo)
        })
        .collect();
    super::attach_stages(&state, &mut rows).await?;
    let refresh = rows.iter().any(PipelineRow::active);
    render(PipelinesPage {
        user,
        nav: RepoNav::new(&repo, role, "pipelines"),
        pipelines: rows,
        show_repo: false,
        prev_url: if page > 0 { format!("/{rp}/-/pipelines?page={}", page - 1) } else { String::new() },
        next_url: if has_next { format!("/{rp}/-/pipelines?page={}", page + 1) } else { String::new() },
        refresh,
    })
}

#[derive(Template)]
#[template(path = "pipeline_new.html")]
struct NewPipelinePage {
    user: User,
    nav: RepoNav,
    branches: Vec<String>,
    tags: Vec<String>,
    selected: String,
    error: String,
    notice: String,
    /// Variables of the selected ref's `.gitlab-ci.yml` that have a `description`.
    form_vars: Vec<FormVariable>,
    /// Why `form_vars` could not be read, if it could not.
    form_vars_error: String,
    vars: Vec<PipelineVar>,
}

#[derive(Deserialize)]
pub struct NewPipelineQuery {
    /// `branch:<name>` or `tag:<name>`, as in the form.
    #[serde(default, rename = "ref")]
    target: Option<String>,
}

pub async fn new_pipeline_form(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Query(q): Query<NewPipelineQuery>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Write).await?;
    if repo.is_mirror() {
        // Fresh branches and variables for the next look; Run fetches again anyway.
        crate::mirror::request_sync(&state, repo.id);
    }
    let selected = q.target.filter(|t| !t.is_empty()).unwrap_or_else(|| repo.default_branch.clone());
    new_pipeline_page(&state, user, &repo, role, selected, String::new(), String::new(), Vec::new()).await
}

/// The name typed or picked in the form, without the `branch:` / `tag:` prefix
/// older forms (and bookmarked links) send. Git ref names cannot contain `:`.
fn ref_input_name(target: &str) -> &str {
    let t = target.trim();
    match t.split_once(':') {
        Some(("branch" | "tag", name)) => name,
        _ => t,
    }
}

/// A branch or tag of the repository: `(ref name, is_tag, sha)`. A bare name is a
/// branch if one exists, else a tag (as in the API); `branch:` / `tag:` pick one.
async fn resolve_target(git: &crate::git::Git, target: &str) -> anyhow::Result<Option<(String, bool, String)>> {
    let name = ref_input_name(target);
    if name.is_empty() {
        return Ok(None);
    }
    let candidates: &[bool] = match target.trim().split_once(':') {
        Some(("branch", _)) => &[false],
        Some(("tag", _)) => &[true],
        _ => &[false, true],
    };
    for &is_tag in candidates {
        let full = format!("refs/{}/{name}", if is_tag { "tags" } else { "heads" });
        if let Some(sha) = git.resolve(&full).await? {
            return Ok(Some((name.to_string(), is_tag, sha)));
        }
    }
    Ok(None)
}

/// The form variables of `.gitlab-ci.yml` at `selected`, or why they could not be read.
async fn load_form_vars(state: &Shared, repo: &Repo, selected: &str) -> (Vec<FormVariable>, String) {
    let git = state.git(repo.id);
    let sha = match resolve_target(&git, selected).await {
        Ok(Some((_, _, sha))) => sha,
        Ok(None) => return (Vec::new(), String::new()),
        Err(e) => return (Vec::new(), format!("{e:#}")),
    };
    match git.blob(&sha, CI_FILE, 1 << 20).await {
        Ok(Some((_, yaml))) => match rust_afariano_ci::form_variables(&String::from_utf8_lossy(&yaml)) {
            Ok(v) => (v, String::new()),
            Err(e) => (Vec::new(), e.to_string()),
        },
        Ok(None) => (Vec::new(), String::new()),
        Err(e) => (Vec::new(), format!("{e:#}")),
    }
}

/// Puts submitted values back into the form variables; the rest stay free rows.
fn merge_form_vars(form_vars: &mut [FormVariable], vars: Vec<PipelineVar>) -> Vec<PipelineVar> {
    let mut extra = Vec::new();
    for v in vars {
        match form_vars.iter_mut().find(|f| f.key == v.key) {
            Some(f) => f.value = v.value,
            None => extra.push(v),
        }
    }
    extra
}

#[allow(clippy::too_many_arguments)]
async fn new_pipeline_page(
    state: &Shared,
    user: User,
    repo: &Repo,
    role: Role,
    selected: String,
    error: String,
    notice: String,
    vars: Vec<PipelineVar>,
) -> AppResult<Response> {
    let refs = state.git(repo.id).refs().await?;
    let (mut form_vars, form_vars_error) = load_form_vars(state, repo, &selected).await;
    let mut vars = merge_form_vars(&mut form_vars, vars);
    vars.retain(|v| !v.key.is_empty() || !v.value.is_empty());
    let free_rows = if form_vars.is_empty() { 3 } else { vars.len() + 1 };
    while vars.len() < free_rows {
        vars.push(PipelineVar { key: String::new(), value: String::new(), secret: None });
    }
    render(NewPipelinePage {
        user,
        nav: RepoNav::new(repo, role, "pipelines"),
        branches: refs.iter().filter(|r| !r.is_tag()).map(|r| r.short().to_string()).collect(),
        tags: refs.iter().filter(|r| r.is_tag()).map(|r| r.short().to_string()).collect(),
        selected: ref_input_name(&selected).to_string(),
        error,
        notice,
        form_vars,
        form_vars_error,
        vars,
    })
}

/// `ref` is a branch or tag name (`branch:` / `tag:` prefixes still work); variables
/// come as parallel arrays.
pub async fn run_pipeline(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Form(form): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Write).await?;
    let mut target = String::new();
    // The ref the prefilled variables were read from, when the form showed any.
    let mut vars_from = None;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    // Row numbers of the ticked "secret" boxes (an unticked box sends nothing).
    let mut secrets = Vec::new();
    for (k, v) in form {
        match k.as_str() {
            "ref" => target = v,
            "vars_from" => vars_from = Some(v),
            "var_key" => keys.push(v),
            "var_value" => values.push(v),
            "var_secret" => secrets.extend(v.parse::<usize>()),
            _ => {}
        }
    }
    let vars: Vec<PipelineVar> = keys
        .into_iter()
        .zip(values)
        .enumerate()
        .map(|(i, (key, value))| PipelineVar { key, value, secret: Some(secrets.contains(&i)) })
        .collect();
    if ref_input_name(&target).is_empty() {
        let msg = "Enter a branch or tag.".to_string();
        return new_pipeline_page(&state, user, &repo, role, target, msg, String::new(), vars).await;
    }
    let vars_clean = match clean_vars(vars.clone()) {
        Ok(v) => v,
        Err(AppError::BadRequest(msg)) => {
            return new_pipeline_page(&state, user, &repo, role, target, msg, String::new(), vars).await;
        }
        Err(e) => return Err(e),
    };
    // A mirror runs what its upstream has now, never what the last sync brought.
    if repo.is_mirror()
        && let Err(e) = crate::mirror::sync_for_pipeline(&state, &repo).await
    {
        let msg = format!(
            "No pipeline was started: pipe could not fetch {}. {e}",
            super::mirror_display(repo.mirror_url.as_deref().unwrap_or_default())
        );
        return new_pipeline_page(&state, user, &repo, role, target, msg, String::new(), vars).await;
    }
    let git = state.git(repo.id);
    let Some((ref_name, is_tag, sha)) = resolve_target(&git, &target).await? else {
        let msg = format!("No branch or tag `{}` in this repository.", ref_input_name(&target));
        return new_pipeline_page(&state, user, &repo, role, target, msg, String::new(), vars).await;
    };
    // The prefilled values came from another ref's .gitlab-ci.yml (the ref was changed
    // and Run clicked before the form reloaded): show this ref's variables instead of
    // running with the other ref's defaults. Values typed in the free rows stay.
    if let Some(from) = vars_from.as_deref().map(ref_input_name).filter(|f| *f != ref_name) {
        let (old, _) = load_form_vars(&state, &repo, from).await;
        let extra: Vec<PipelineVar> = vars.into_iter().filter(|v| !old.iter().any(|f| f.key == v.key)).collect();
        let notice = format!("The variables below now come from `{ref_name}`. Check them, then run the pipeline.");
        return new_pipeline_page(&state, user, &repo, role, target, String::new(), notice, extra).await;
    }
    let np = NewPipeline {
        repo: &repo,
        ref_name: ref_name.clone(),
        is_tag,
        sha,
        before_sha: crate::git::ZERO_SHA.into(),
        source: "web",
        user: Some(&user),
        variables: vars_clean,
    };
    let msg = match pipelines::create(&state, np).await? {
        Created::Pipeline(iid) => {
            return Ok(Redirect::to(&format!("/{}/-/pipelines/{iid}", repo.path())).into_response());
        }
        Created::Filtered(reason) => format!("No pipeline was created: {reason}."),
        Created::NoConfig => format!("There is no {CI_FILE} on `{ref_name}`."),
    };
    new_pipeline_page(&state, user, &repo, role, target, msg, String::new(), vars).await
}

struct StageCol {
    name: String,
    jobs: Vec<JobRow>,
}

#[derive(Template)]
#[template(path = "pipeline.html")]
struct PipelinePage {
    user: User,
    nav: RepoNav,
    p: PipelineRow,
    error: String,
    stages: Vec<StageCol>,
    variables: Vec<String>,
    commit_url: String,
    created_at: String,
    can_cancel: bool,
    can_retry: bool,
    refresh: bool,
}

async fn load_pipeline(state: &Shared, repo: &Repo, iid: i32) -> AppResult<Pipeline> {
    sqlx::query_as(sql!("SELECT {PIPELINE_COLS} FROM pipelines WHERE repo_id = $1 AND iid = $2"))
        .bind(repo.id)
        .bind(iid)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn pipeline(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, iid)): Path<(String, String, i32)>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let p = load_pipeline(&state, &repo, iid).await?;
    let jobs: Vec<Job> = sqlx::query_as(sql!(
        "SELECT {JOB_COLS} FROM jobs WHERE pipeline_id = $1 AND NOT retried ORDER BY stage_idx, id"
    ))
    .bind(p.id)
    .fetch_all(&state.db)
    .await?;
    let rp = repo.path();
    let names = user_names(&state, p.created_by.into_iter()).await?;
    let row = PipelineRow::new(&p, &rp, p.created_by.and_then(|id| names.get(&id)).map(String::as_str));
    let mut stages: Vec<StageCol> = Vec::new();
    for (idx, stage) in p.stages.iter().enumerate() {
        let col: Vec<JobRow> =
            jobs.iter().filter(|j| j.stage_idx as usize == idx).map(|j| JobRow::new(j, &rp)).collect();
        if !col.is_empty() {
            stages.push(StageCol { name: stage.clone(), jobs: col });
        }
    }
    let can_cancel = jobs.iter().any(|j| matches!(j.status.as_str(), "created" | "pending" | "running" | "manual"));
    let can_retry = jobs.iter().any(|j| matches!(j.status.as_str(), "failed" | "canceled"));
    let refresh = row.active() || jobs.iter().any(|j| matches!(j.status.as_str(), "pending" | "running"));
    render(PipelinePage {
        user,
        nav: RepoNav::new(&repo, role, "pipelines"),
        error: p.error.clone().unwrap_or_default(),
        variables: p.variables.0.iter().map(|v| v.key.clone()).collect(),
        commit_url: format!("/{rp}/-/commit/{}", p.sha),
        created_at: p.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
        p: row,
        stages,
        can_cancel,
        can_retry,
        refresh,
    })
}

pub async fn pipeline_action(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, iid, action)): Path<(String, String, i32, String)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Write).await?;
    let p = load_pipeline(&state, &repo, iid).await?;
    match action.as_str() {
        "cancel" => pipelines::cancel_pipeline(&state, p.id).await?,
        "retry" => pipelines::retry_pipeline(&state, p.id, &user).await?,
        _ => return Err(AppError::NotFound),
    }
    Ok(Redirect::to(&format!("/{}/-/pipelines/{iid}", repo.path())).into_response())
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobPage {
    user: User,
    nav: RepoNav,
    job: JobRow,
    pipeline: PipelineRow,
    runner: String,
    image: String,
    created: String,
    started: String,
    queued_for: String,
    exit_code: String,
    tags: Vec<String>,
    stream_url: String,
    raw_url: String,
    retried_from: String,
    cancel_requested: bool,
}

async fn load_job(state: &Shared, repo: &Repo, id: i64) -> AppResult<Job> {
    let job = pipelines::get_job(state, id).await?;
    if job.repo_id != repo.id {
        return Err(AppError::NotFound);
    }
    Ok(job)
}

pub async fn job(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, id)): Path<(String, String, i64)>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let job = load_job(&state, &repo, id).await?;
    let p = pipelines::get_pipeline(&state, job.pipeline_id).await?;
    let rp = repo.path();
    let runner = match job.runner_id {
        Some(rid) => sqlx::query_as::<_, Runner>(sql!("SELECT {RUNNER_COLS} FROM runners WHERE id = $1"))
            .bind(rid)
            .fetch_optional(&state.db)
            .await?
            .map(|r| format!("{} ({}, {})", r.name, r.executor, r.platform))
            .unwrap_or_else(|| format!("#{rid} (deleted)")),
        None => String::new(),
    };
    let retried_from: Option<i64> =
        sqlx::query_scalar("SELECT id FROM jobs WHERE retry_of = $1").bind(job.id).fetch_optional(&state.db).await?;
    let spec = &job.spec.0;
    render(JobPage {
        user,
        nav: RepoNav::new(&repo, role, "pipelines"),
        pipeline: PipelineRow::new(&p, &rp, None),
        runner,
        image: spec.image.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
        created: ago(job.created_at),
        started: job.started_at.map(ago).unwrap_or_default(),
        queued_for: match (job.queued_at, job.started_at) {
            (Some(q), Some(s)) => crate::util::duration((s - q).num_seconds()),
            _ => String::new(),
        },
        exit_code: job.exit_code.map(|c| c.to_string()).unwrap_or_default(),
        tags: job.tags.clone(),
        stream_url: format!("/{rp}/-/jobs/{}/stream", job.id),
        raw_url: format!("/{rp}/-/jobs/{}/raw", job.id),
        retried_from: retried_from.map(|n| format!("/{rp}/-/jobs/{n}")).unwrap_or_default(),
        cancel_requested: job.cancel_requested && job.status == "running",
        job: JobRow::new(&job, &rp),
    })
}

pub async fn job_raw(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, id)): Path<(String, String, i64)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let job = load_job(&state, &repo, id).await?;
    let data = state.logs.read(job.id, 0, (rust_afariano_proto::MAX_LOG_SIZE + 4096) as usize).await?;
    let text = strip_ansi(&String::from_utf8_lossy(&data));
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub async fn job_action(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, id, action)): Path<(String, String, i64, String)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Write).await?;
    let job = load_job(&state, &repo, id).await?;
    let target = match action.as_str() {
        "play" => {
            pipelines::play_job(&state, &job, &user).await?;
            job.id
        }
        "cancel" => {
            pipelines::cancel_job(&state, &job).await?;
            job.id
        }
        "retry" => pipelines::retry_job(&state, &job, &user).await.map_err(|e| bad(e.to_string()))?,
        _ => return Err(AppError::NotFound),
    };
    Ok(Redirect::to(&format!("/{}/-/jobs/{target}", repo.path())).into_response())
}

/// Live log: everything already written, then new bytes as the runner sends
/// them, then an `end` event with the final status.
pub async fn job_stream(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, id)): Path<(String, String, i64)>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let job = load_job(&state, &repo, id).await?;
    let (tx, rx) = mpsc::channel::<Event>(32);
    tokio::spawn(stream_log(state, job, tx));
    Ok(Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// Sends text up to the last complete UTF-8 character; keeps the rest in `carry`.
fn utf8_chunk(carry: &mut Vec<u8>) -> Option<String> {
    if carry.is_empty() {
        return None;
    }
    let valid = match std::str::from_utf8(carry) {
        Ok(_) => carry.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => carry.len(),
    };
    if valid == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&carry[..valid]).into_owned();
    carry.drain(..valid);
    Some(text)
}

async fn stream_log(state: Shared, job: Job, tx: mpsc::Sender<Event>) {
    const CHUNK: usize = 256 << 10;
    const INITIAL_TAIL: u64 = 1 << 20;
    let mut rx = state.logs.subscribe(job.id);
    let started_terminal = job.is_terminal();
    let size = state.logs.size(job.id).await;
    let mut pos = 0u64;
    let mut carry: Vec<u8> = Vec::new();
    if size > 2 * INITIAL_TAIL {
        pos = size - INITIAL_TAIL;
        let note =
            format!("\x1b[33;1m… {} MiB skipped, open the raw log for everything …\x1b[0m\n", (pos >> 20).max(1));
        let _ = tx.send(log_event(&note)).await;
    }
    macro_rules! catch_up {
        () => {
            loop {
                let Ok(data) = state.logs.read(job.id, pos, CHUNK).await else { break };
                if data.is_empty() {
                    break;
                }
                pos += data.len() as u64;
                carry.extend_from_slice(&data);
                if let Some(t) = utf8_chunk(&mut carry) {
                    if tx.send(log_event(&t)).await.is_err() {
                        return;
                    }
                }
            }
        };
    }
    catch_up!();
    let mut finished = started_terminal;
    while !finished {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(LogEvent::Chunk { offset, data })) => {
                let end = offset + data.len() as u64;
                if end <= pos {
                    continue;
                }
                if offset > pos {
                    catch_up!();
                    continue;
                }
                carry.extend_from_slice(&data[(pos - offset) as usize..]);
                pos = end;
                if let Some(t) = utf8_chunk(&mut carry)
                    && tx.send(log_event(&t)).await.is_err()
                {
                    return;
                }
            }
            Ok(Ok(LogEvent::End)) | Ok(Err(broadcast::error::RecvError::Closed)) => finished = true,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => catch_up!(),
            Err(_) => {
                // Quiet: check whether the job ended without us hearing about it.
                match pipelines::get_job(&state, job.id).await {
                    Ok(j) if is_terminal(&j.status) => finished = true,
                    Ok(_) => {}
                    Err(_) => finished = true,
                }
            }
        }
    }
    catch_up!();
    if !carry.is_empty() {
        let _ = tx.send(log_event(&String::from_utf8_lossy(&carry))).await;
    }
    let status = pipelines::get_job(&state, job.id).await.map(|j| j.status).unwrap_or_default();
    let end = serde_json::json!({ "status": status, "reload": !started_terminal });
    let _ = tx.send(Event::default().event("end").data(end.to_string())).await;
}

fn log_event(text: &str) -> Event {
    Event::default().event("log").data(serde_json::json!({ "t": text }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_carry() {
        let mut c = "aé".as_bytes().to_vec();
        let last = c.pop().unwrap();
        assert_eq!(utf8_chunk(&mut c).as_deref(), Some("a"));
        c.push(last);
        assert_eq!(utf8_chunk(&mut c).as_deref(), Some("é"));
        assert!(c.is_empty());
    }

    #[test]
    fn ref_input_accepts_plain_and_prefixed_names() {
        assert_eq!(ref_input_name("main"), "main");
        assert_eq!(ref_input_name("  release/2.1 "), "release/2.1");
        assert_eq!(ref_input_name("branch:develop"), "develop");
        assert_eq!(ref_input_name("tag:v1.0"), "v1.0");
        assert_eq!(ref_input_name(""), "");
    }

    #[test]
    fn ansi_strip() {
        assert_eq!(strip_ansi("\x1b[32;1m$ echo\x1b[0m hi"), "$ echo hi");
    }

    fn test_repo(list_shows_variables: bool) -> Repo {
        Repo {
            id: 1,
            namespace: "acme".into(),
            name: "web".into(),
            description: String::new(),
            default_branch: "main".into(),
            protected_branches: "main".into(),
            pipelines_on_push: true,
            created_at: chrono::Utc::now(),
            pushed_at: None,
            mirror_url: None,
            prevent_outdated_deploys: true,
            list_shows_variables,
        }
    }

    fn pipelines_page(repo: &Repo, role: Role, pipelines: Vec<PipelineRow>) -> String {
        PipelinesPage {
            user: User { id: 2, email: "ana@example.com".into(), name: String::new(), is_admin: false },
            nav: RepoNav::new(repo, role, "pipelines"),
            pipelines,
            show_repo: false,
            prev_url: String::new(),
            next_url: String::new(),
            refresh: false,
        }
        .render()
        .unwrap()
    }

    /// With the setting on, a row shows its run variables and the commit hash in place of
    /// the commit title; a secret value is not in the page at all.
    #[test]
    fn pipeline_list_shows_variables() {
        let pipeline = crate::models::Pipeline {
            id: 1,
            repo_id: 1,
            iid: 7,
            ref_name: "main".into(),
            ref_is_tag: false,
            sha: "0123abcd4567ef890123abcd4567ef890123abcd".into(),
            before_sha: String::new(),
            source: "web".into(),
            status: "success".into(),
            variables: sqlx::types::Json(vec![
                PipelineVar { key: "TARGET_ENV".into(), value: "staging".into(), secret: Some(false) },
                PipelineVar { key: "DEPLOY_TOKEN".into(), value: "s3cr3t-value".into(), secret: Some(true) },
            ]),
            stages: Vec::new(),
            yaml_vars: sqlx::types::Json(Vec::new()),
            error: None,
            commit_title: "Fix the login form".into(),
            created_by: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
        };
        let html = |on| {
            let repo = test_repo(on);
            pipelines_page(&repo, Role::Read, vec![PipelineRow::new(&pipeline, "acme/web", None).for_repo(&repo)])
        };
        let on = html(true);
        assert!(on.contains("TARGET_ENV") && on.contains("staging"));
        assert!(on.contains("DEPLOY_TOKEN") && !on.contains("s3cr3t-value"));
        assert!(on.contains("0123abcd"), "the commit hash stays");
        assert!(!on.contains("Fix the login form"));
        let off = html(false);
        assert!(off.contains("Fix the login form") && off.contains("0123abcd"));
        assert!(!off.contains("staging") && !off.contains("s3cr3t-value"));
    }

    /// Readers see no Run pipeline button and no Settings tab; maintainers see both.
    #[test]
    fn pipelines_page_follows_role() {
        let repo = test_repo(false);
        let page = |role| pipelines_page(&repo, role, Vec::new());
        let read = page(Role::Read);
        assert!(read.contains(r#"href="https://github.com/frank10gm/rust-afariano""#), "sidebar links the repository");
        assert!(!read.contains("/acme/web/-/pipelines/new"));
        assert!(!read.contains("/acme/web/-/settings"));
        let write = page(Role::Write);
        assert!(write.contains("/acme/web/-/pipelines/new"));
        assert!(!write.contains("/acme/web/-/settings"));
        let maintain = page(Role::Maintain);
        assert!(maintain.contains("/acme/web/-/pipelines/new"));
        assert!(maintain.contains("/acme/web/-/settings"));
        // Assets carry the version, so an upgrade is not hidden by a cached copy.
        let v = crate::web::ASSET_V;
        assert!(maintain.contains(&format!("/-/static/app.css?v={v}\"")));
        assert!(maintain.contains(&format!("/-/static/app.js?v={v}\"")));
        assert!(maintain.contains(&format!("/-/static/icons.svg?v={v}#i-")));
        assert!(!maintain.contains("/-/static/icons.svg#"));
    }
}
