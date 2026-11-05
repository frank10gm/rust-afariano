//! JSON API for scripts, authenticated with a personal access token
//! (`Authorization: Bearer rfp_…`). Errors are `{"message": …}`.
//! The same rules as the web UI apply: repository endpoints need the caller's
//! role in that repository, user and runner endpoints need an administrator.
//! Documented in docs/API.md.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::access::{self, Role};
use crate::app::{AppState, Shared};
use crate::auth::TokenUser2;
use crate::error::{AppError, AppResult, bad};
use crate::models::{JOB_COLS, Job, PIPELINE_COLS, Pipeline, RUNNER_COLS, Repo, Runner, User, Variable};
use crate::{ops, pipelines};

fn require_admin(user: &User) -> AppResult<()> {
    if user.is_admin { Ok(()) } else { Err(AppError::Forbidden) }
}

// ---------------------------------------------------------------- repositories

fn repo_json(state: &AppState, repo: &Repo, role: Role) -> Value {
    let path = repo.path();
    let cfg = &state.cfg;
    json!({
        "path": path,
        "description": repo.description,
        "default_branch": repo.default_branch,
        "protected_branches": repo.protected_branches.split(',').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>(),
        "pipelines_on_push": repo.pipelines_on_push,
        "prevent_outdated_deploys": repo.prevent_outdated_deploys,
        "list_shows_variables": repo.list_shows_variables,
        "mirror_url": repo.mirror_url,
        "role": role.as_str(),
        "web_url": format!("{}/{path}", cfg.public_url),
        "http_url": cfg.http_clone_url(&path),
        "ssh_url": cfg.ssh_clone_url(&path),
    })
}

#[derive(Deserialize)]
pub struct NewRepo {
    namespace: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    protected_branches: Option<Vec<String>>,
    #[serde(default)]
    pipelines_on_push: Option<bool>,
    #[serde(default)]
    mirror: Option<MirrorBody>,
}

/// `POST /api/v1/repos` `{"namespace", "name", "description"?, "default_branch"?,
/// "protected_branches"?, "pipelines_on_push"?, "mirror"?}` (administrators).
/// `mirror` (`{"url", "branches"?, "https_token"?, "status_token"?}`) makes it a pull mirror.
pub async fn create_repo(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Json(b): Json<NewRepo>,
) -> AppResult<Response> {
    require_admin(&user)?;
    let ns = b.namespace.trim();
    let name = b.name.trim();
    let branch = b.default_branch.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("main");
    if let Some(m) = &b.mirror {
        crate::mirror::Upstream::parse(&m.url).map_err(bad)?;
        if let Some(br) = &m.branches {
            crate::mirror::parse_branches(br).map_err(bad)?;
        }
    }
    let id = ops::create_repo(&state, &user, ns, name, &b.description, branch).await?.map_err(bad)?;
    let mut repo = state.repo_by_id(id).await?;
    if let Some(m) = b.mirror {
        crate::mirror::configure(&state, &repo, m.into_input()).await?.map_err(bad)?;
        crate::mirror::request_sync(&state, id);
        repo = state.repo_by_id(id).await?;
    }
    if b.protected_branches.is_some() || b.pipelines_on_push.is_some() {
        let mut s = ops::RepoSettings::of(&repo);
        if let Some(p) = b.protected_branches {
            s.protected_branches = p;
        }
        if let Some(on) = b.pipelines_on_push {
            s.pipelines_on_push = on;
        }
        ops::update_repo(&state, &repo, s).await?.map_err(bad)?;
        repo = state.repo_by_id(id).await?;
    }
    Ok((StatusCode::CREATED, Json(repo_json(&state, &repo, Role::Maintain))).into_response())
}

/// `GET /api/v1/repos/{ns}/{name}` — settings and clone URLs (read).
pub async fn get_repo(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (repo, role) = access::repo(&state, &user, &ns, &name, Role::Read).await?;
    Ok(Json(repo_json(&state, &repo, role)))
}

#[derive(Deserialize)]
pub struct RepoPatch {
    description: Option<String>,
    default_branch: Option<String>,
    protected_branches: Option<Vec<String>>,
    pipelines_on_push: Option<bool>,
    prevent_outdated_deploys: Option<bool>,
    list_shows_variables: Option<bool>,
}

/// `PATCH /api/v1/repos/{ns}/{name}` — changes only the fields given (maintain).
pub async fn patch_repo(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
    Json(b): Json<RepoPatch>,
) -> AppResult<Json<Value>> {
    let (repo, role) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let mut s = ops::RepoSettings::of(&repo);
    if let Some(v) = b.description {
        s.description = v;
    }
    if let Some(v) = b.default_branch {
        s.default_branch = v;
    }
    if let Some(v) = b.protected_branches {
        s.protected_branches = v;
    }
    if let Some(v) = b.pipelines_on_push {
        s.pipelines_on_push = v;
    }
    if let Some(v) = b.prevent_outdated_deploys {
        s.prevent_outdated_deploys = v;
    }
    if let Some(v) = b.list_shows_variables {
        s.list_shows_variables = v;
    }
    ops::update_repo(&state, &repo, s).await?.map_err(bad)?;
    tracing::info!(repo = %repo.path(), by = %user.email, "repository settings changed");
    let repo = state.repo_by_id(repo.id).await?;
    Ok(Json(repo_json(&state, &repo, role)))
}

// ---------------------------------------------------------------- mirrors

#[derive(Deserialize)]
pub struct MirrorBody {
    url: String,
    #[serde(default)]
    branches: Option<String>,
    /// `""` removes it; absent keeps it.
    #[serde(default)]
    https_token: Option<String>,
    #[serde(default)]
    status_token: Option<String>,
}

impl MirrorBody {
    fn into_input(self) -> crate::mirror::MirrorInput {
        crate::mirror::MirrorInput {
            url: self.url,
            branches: self.branches,
            https_token: self.https_token,
            status_token: self.status_token,
        }
    }
}

async fn mirror_json(state: &AppState, repo: &Repo) -> AppResult<Value> {
    let url = repo.mirror_url.clone().ok_or_else(|| bad("this repository is not a mirror"))?;
    let m = crate::mirror::load(state, repo.id).await?.ok_or(AppError::NotFound)?;
    let up = crate::mirror::Upstream::parse(&url).ok();
    let ssh = up.as_ref().is_some_and(|u| u.transport == crate::mirror::Transport::Ssh);
    Ok(json!({
        "url": url,
        "upstream": up.as_ref().map(|u| u.display()),
        "ssh_public_key": ssh.then_some(&m.ssh_public_key),
        "host_keys": crate::mirror::host_fingerprints(&m.known_hosts),
        "branches": m.branches,
        "has_https_token": m.https_token.is_some(),
        "synced_at": m.synced_at,
        "checked_at": m.checked_at,
        "error": m.error,
        "webhook_url": crate::mirror::webhook_url(state, repo),
        "webhook_secret": state.cipher.decrypt(&m.webhook_secret)?,
        "hook_at": m.hook_at,
        "hook_error": m.hook_error,
        "forge": up.as_ref().and_then(|u| u.forge()).map(|f| f.name()),
        "has_status_token": m.status_token.is_some(),
        "status_at": m.status_at,
        "status_error": m.status_error,
    }))
}

/// `GET /api/v1/repos/{ns}/{name}/mirror` — key, sync state, webhook (maintain).
pub async fn get_mirror(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    Ok(Json(mirror_json(&state, &repo).await?))
}

/// `PUT /api/v1/repos/{ns}/{name}/mirror` `{"url", "branches"?, "https_token"?, "status_token"?}`
/// — makes the repository a mirror, or changes its mirror settings. Maintainers may
/// change everything but the URL, which is for administrators.
pub async fn put_mirror(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
    Json(b): Json<MirrorBody>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    if repo.mirror_url.as_deref() != Some(b.url.trim()) {
        require_admin(&user)?;
    }
    crate::mirror::configure(&state, &repo, b.into_input()).await?.map_err(bad)?;
    tracing::info!(repo = %repo.path(), by = %user.email, "mirror settings saved");
    crate::mirror::request_sync(&state, repo.id);
    let repo = state.repo_by_id(repo.id).await?;
    Ok(Json(mirror_json(&state, &repo).await?))
}

/// `DELETE /api/v1/repos/{ns}/{name}/mirror` — an ordinary repository again (administrators).
pub async fn delete_mirror(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    require_admin(&user)?;
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    crate::mirror::stop(&state, &repo).await?;
    tracing::warn!(repo = %repo.path(), by = %user.email, "mirroring stopped");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/repos/{ns}/{name}/mirror/sync` — fetches the upstream now (write).
pub async fn sync_mirror(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Write).await?;
    if !repo.is_mirror() {
        return Err(bad("this repository is not a mirror"));
    }
    let synced = crate::mirror::sync_for_pipeline(&state, &repo).await.map_err(AppError::BadGateway)?;
    let m = crate::mirror::load(&state, repo.id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(json!({ "changed": synced.changed, "synced_at": m.synced_at })))
}

/// `POST /api/v1/repos/{ns}/{name}/mirror/secret` — a new webhook secret (maintain).
pub async fn new_mirror_secret(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    if !repo.is_mirror() {
        return Err(bad("this repository is not a mirror"));
    }
    let secret = crate::mirror::new_webhook_secret(&state, repo.id).await?;
    Ok(Json(json!({ "webhook_secret": secret })))
}

/// `POST /api/v1/repos/{ns}/{name}/mirror/hook` — the upstream's webhook. No token:
/// the call is signed with the mirror's secret (GitHub, Bitbucket) or carries it
/// (GitLab). Its content is not read; it only queues a sync. Every refusal answers
/// 404, like a repository that does not exist; Settings → Mirror shows why.
pub async fn mirror_hook(
    State(state): State<Shared>,
    Path((ns, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> AppResult<Response> {
    let repo = state.repo_by_path(&ns, &name).await?.ok_or(AppError::NotFound)?;
    if !repo.is_mirror() {
        return Err(AppError::NotFound);
    }
    let m = crate::mirror::load(&state, repo.id).await?.ok_or(AppError::NotFound)?;
    let secret = state.cipher.decrypt(&m.webhook_secret)?;
    if let Err(why) = crate::mirror::verify_webhook(&secret, &headers, &body) {
        tracing::warn!(repo = %repo.path(), "mirror webhook refused: {why}");
        crate::mirror::record_hook(&state, repo.id, Some(why)).await;
        return Err(AppError::NotFound);
    }
    crate::mirror::record_hook(&state, repo.id, None).await;
    crate::mirror::request_sync(&state, repo.id);
    Ok((StatusCode::ACCEPTED, Json(json!({ "message": "sync queued" }))).into_response())
}

// ---------------------------------------------------------------- members

/// An account by id (digits) or by email.
async fn find_account(state: &AppState, key: &str) -> AppResult<(i64, String, String, bool, bool)> {
    let row: Option<(i64, String, String, bool, bool)> = match key.parse::<i64>() {
        Ok(id) => {
            sqlx::query_as("SELECT id, email, name, is_admin, disabled FROM users WHERE id = $1")
                .bind(id)
                .fetch_optional(&state.db)
                .await?
        }
        Err(_) => {
            sqlx::query_as("SELECT id, email, name, is_admin, disabled FROM users WHERE lower(email) = lower($1)")
                .bind(key.trim())
                .fetch_optional(&state.db)
                .await?
        }
    };
    row.ok_or_else(|| bad(format!("no account `{key}`")))
}

/// `GET /api/v1/repos/{ns}/{name}/members` (maintain).
pub async fn list_members(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let rows: Vec<(i64, String, String, String, bool)> = sqlx::query_as(
        "SELECT u.id, u.email, u.name, m.role, u.is_admin FROM repo_members m JOIN users u ON u.id = m.user_id \
         WHERE m.repo_id = $1 ORDER BY lower(u.email)",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!(
        rows.into_iter()
            .map(|(id, email, name, role, admin)| json!({
                "user_id": id, "email": email, "name": name, "role": role, "admin": admin,
            }))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
pub struct MemberBody {
    role: String,
}

/// `PUT /api/v1/repos/{ns}/{name}/members/{user}` `{"role": "read"|"write"|"maintain"}`
/// — adds the account (id or email) or changes its role (maintain).
pub async fn put_member(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, who)): Path<(String, String, String)>,
    Json(b): Json<MemberBody>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let role = Role::parse(&b.role).ok_or_else(|| bad("role must be read, write or maintain"))?;
    let (id, email, member_name, admin, _) = find_account(&state, &who).await?;
    ops::set_member(&state, repo.id, id, role).await?;
    tracing::info!(repo = %repo.path(), member = %email, role = role.as_str(), by = %user.email, "member saved");
    Ok(Json(json!({ "user_id": id, "email": email, "name": member_name, "role": role.as_str(), "admin": admin })))
}

/// `DELETE /api/v1/repos/{ns}/{name}/members/{user}` (maintain).
pub async fn delete_member(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, who)): Path<(String, String, String)>,
) -> AppResult<StatusCode> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let (id, email, ..) = find_account(&state, &who).await?;
    let n = sqlx::query("DELETE FROM repo_members WHERE repo_id = $1 AND user_id = $2")
        .bind(repo.id)
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(repo = %repo.path(), member = %email, by = %user.email, "member removed");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- variables

#[derive(Deserialize)]
pub struct VariableBody {
    value: String,
    #[serde(default)]
    file: bool,
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    masked: bool,
    #[serde(default)]
    environment_scope: String,
}

/// `PUT /api/v1/repos/{ns}/{name}/variables/{key}` — create or replace (per scope) (maintain).
pub async fn put_variable(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, key)): Path<(String, String, String)>,
    Json(b): Json<VariableBody>,
) -> AppResult<StatusCode> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let input = ops::VariableInput {
        key,
        value: b.value,
        file: b.file,
        protected: b.protected,
        masked: b.masked,
        environment_scope: b.environment_scope,
    };
    let key = ops::upsert_variable(&state, repo.id, input).await?.map_err(bad)?;
    tracing::info!(repo = %repo.path(), key = %key, by = %user.email, "variable saved");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/repos/{ns}/{name}/variables` — keys and flags, never values (maintain).
pub async fn list_variables(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let vars: Vec<Variable> = sqlx::query_as(
        "SELECT id, repo_id, key, '' AS value, file, protected, masked, environment_scope FROM variables \
         WHERE repo_id = $1 ORDER BY key, environment_scope",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!(
        vars.iter()
            .map(|v| json!({
                "key": v.key, "environment_scope": v.environment_scope,
                "file": v.file, "protected": v.protected, "masked": v.masked,
            }))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
pub struct ScopeQuery {
    environment_scope: Option<String>,
}

/// `DELETE /api/v1/repos/{ns}/{name}/variables/{key}?environment_scope=…` (default `*`) (maintain).
pub async fn delete_variable(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, key)): Path<(String, String, String)>,
    Query(q): Query<ScopeQuery>,
) -> AppResult<StatusCode> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let scope = q.environment_scope.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("*");
    let n = sqlx::query("DELETE FROM variables WHERE repo_id = $1 AND key = $2 AND environment_scope = $3")
        .bind(repo.id)
        .bind(&key)
        .bind(scope)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(repo = %repo.path(), key = %key, by = %user.email, "variable deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- pipelines and jobs

async fn pipeline_json(state: &AppState, repo: &Repo, p: &Pipeline) -> AppResult<Value> {
    let jobs: Vec<Job> = sqlx::query_as(sql!(
        "SELECT {JOB_COLS} FROM jobs WHERE pipeline_id = $1 AND NOT retried ORDER BY stage_idx, id"
    ))
    .bind(p.id)
    .fetch_all(&state.db)
    .await?;
    let base = format!("{}/{}", state.cfg.public_url, repo.path());
    Ok(json!({
        "iid": p.iid,
        "status": p.status,
        "ref": p.ref_name,
        "sha": p.sha,
        "source": p.source,
        "error": p.error,
        "web_url": format!("{base}/-/pipelines/{}", p.iid),
        "jobs": jobs.iter().map(|j| json!({
            "id": j.id,
            "name": j.name,
            "stage": j.stage,
            "status": j.status,
            "when": j.when,
            "environment": j.environment,
            "failure_reason": j.failure_reason,
            "web_url": format!("{base}/-/jobs/{}", j.id),
        })).collect::<Vec<_>>(),
    }))
}

async fn load_pipeline(state: &AppState, repo: &Repo, iid: i32) -> AppResult<Pipeline> {
    sqlx::query_as(sql!("SELECT {PIPELINE_COLS} FROM pipelines WHERE repo_id = $1 AND iid = $2"))
        .bind(repo.id)
        .bind(iid)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// `GET /api/v1/repos/{ns}/{name}/pipelines/{iid}` — status and jobs (read).
pub async fn get_pipeline(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, iid)): Path<(String, String, i32)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Read).await?;
    let p = load_pipeline(&state, &repo, iid).await?;
    Ok(Json(pipeline_json(&state, &repo, &p).await?))
}

/// `POST /api/v1/repos/{ns}/{name}/pipelines/{iid}/{retry|cancel}` (write).
pub async fn pipeline_action(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, iid, action)): Path<(String, String, i32, String)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Write).await?;
    let p = load_pipeline(&state, &repo, iid).await?;
    match action.as_str() {
        "cancel" => pipelines::cancel_pipeline(&state, p.id).await?,
        "retry" => pipelines::retry_pipeline(&state, p.id, &user).await.map_err(|e| bad(e.to_string()))?,
        _ => return Err(AppError::NotFound),
    }
    let p = load_pipeline(&state, &repo, iid).await?;
    Ok(Json(pipeline_json(&state, &repo, &p).await?))
}

async fn load_job(state: &AppState, repo: &Repo, id: i64) -> AppResult<Job> {
    let job = pipelines::get_job(state, id).await?;
    if job.repo_id != repo.id {
        return Err(AppError::NotFound);
    }
    Ok(job)
}

/// `GET /api/v1/repos/{ns}/{name}/jobs/{id}/log` — the log as plain text, colours removed (read).
pub async fn job_log(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, id)): Path<(String, String, i64)>,
) -> AppResult<Response> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Read).await?;
    let job = load_job(&state, &repo, id).await?;
    let data = state.logs.read(job.id, 0, (rust_afariano_proto::MAX_LOG_SIZE + 4096) as usize).await?;
    let text = crate::web::strip_ansi(&String::from_utf8_lossy(&data));
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
            (header::HeaderName::from_static("x-job-status"), job.status.clone()),
        ],
        text,
    )
        .into_response())
}

/// `POST /api/v1/repos/{ns}/{name}/jobs/{id}/{play|retry|cancel}` (write). Answers
/// with the job (for `retry`, the new job).
pub async fn job_action(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path((ns, name, id, action)): Path<(String, String, i64, String)>,
) -> AppResult<Json<Value>> {
    let (repo, _) = access::repo(&state, &user, &ns, &name, Role::Write).await?;
    let job = load_job(&state, &repo, id).await?;
    let target = match action.as_str() {
        "play" => {
            if job.status != "manual" {
                return Err(bad(format!("job {} is {}, not manual", job.id, job.status)));
            }
            pipelines::play_job(&state, &job, &user).await?;
            job.id
        }
        "cancel" => {
            if job.is_terminal() {
                return Err(bad(format!("job {} already finished ({})", job.id, job.status)));
            }
            pipelines::cancel_job(&state, &job).await?;
            job.id
        }
        "retry" => pipelines::retry_job(&state, &job, &user).await.map_err(|e| bad(e.to_string()))?,
        _ => return Err(AppError::NotFound),
    };
    tracing::info!(repo = %repo.path(), job = job.id, action = %action, by = %user.email, "job action");
    let j = pipelines::get_job(&state, target).await?;
    Ok(Json(json!({
        "id": j.id,
        "name": j.name,
        "status": j.status,
        "web_url": format!("{}/{}/-/jobs/{}", state.cfg.public_url, repo.path(), j.id),
    })))
}

// ---------------------------------------------------------------- users (administrators)

/// `GET /api/v1/users` (administrators).
pub async fn list_users(State(state): State<Shared>, TokenUser2(user): TokenUser2) -> AppResult<Json<Value>> {
    require_admin(&user)?;
    let rows: Vec<(i64, String, String, bool, bool)> =
        sqlx::query_as("SELECT id, email, name, is_admin, disabled FROM users ORDER BY lower(email)")
            .fetch_all(&state.db)
            .await?;
    Ok(Json(json!(
        rows.into_iter()
            .map(|(id, email, name, admin, disabled)| json!({
                "id": id, "email": email, "name": name, "admin": admin, "disabled": disabled,
            }))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
pub struct NewUser {
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    admin: bool,
}

/// `POST /api/v1/users` `{"email", "name"?, "password"?, "admin"?}` (administrators).
/// Without a password the account signs in through SSO only, or not at all.
pub async fn create_user(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Json(b): Json<NewUser>,
) -> AppResult<Response> {
    require_admin(&user)?;
    let password = b.password.as_deref().map(|p| (p, p));
    let id = ops::create_account(&state, &user, &b.email, &b.name, password, b.admin).await?.map_err(bad)?;
    let (id, email, name, admin, disabled) = find_account(&state, &id.to_string()).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "email": email, "name": name, "admin": admin, "disabled": disabled })),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct NewToken {
    name: String,
    #[serde(default)]
    expires_days: Option<i64>,
}

/// `POST /api/v1/users/{user}/tokens` `{"name", "expires_days"?}` → `{"token"}`, shown
/// once (administrators). For service accounts: the token acts as that account.
/// Another administrator's tokens are refused; they create their own.
pub async fn create_user_token(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path(who): Path<String>,
    Json(b): Json<NewToken>,
) -> AppResult<Response> {
    require_admin(&user)?;
    let (id, email, _, admin, disabled) = find_account(&state, &who).await?;
    if admin && id != user.id {
        return Err(bad("another administrator's tokens can only be created by them"));
    }
    if disabled {
        return Err(bad(format!("{email} is disabled")));
    }
    let token = ops::create_token(&state, id, &b.name, b.expires_days).await?.map_err(bad)?;
    tracing::warn!(user = %email, by = %user.email, name = %b.name.trim(), "access token created for another account");
    Ok((StatusCode::CREATED, Json(json!({ "user_id": id, "email": email, "name": b.name.trim(), "token": token })))
        .into_response())
}

// ---------------------------------------------------------------- runners (administrators)

fn runner_json(r: &Runner) -> Value {
    json!({
        "id": r.id,
        "name": r.name,
        "tags": r.tags,
        "run_untagged": r.run_untagged,
        "paused": r.paused,
        "online": r.online(),
        "contacted_at": r.contacted_at,
        "hostname": r.hostname,
        "platform": r.platform,
        "executor": r.executor,
        "version": r.version,
    })
}

/// `GET /api/v1/runners` (administrators).
pub async fn list_runners(State(state): State<Shared>, TokenUser2(user): TokenUser2) -> AppResult<Json<Value>> {
    require_admin(&user)?;
    let runners: Vec<Runner> =
        sqlx::query_as(sql!("SELECT {RUNNER_COLS} FROM runners ORDER BY name")).fetch_all(&state.db).await?;
    Ok(Json(json!(runners.iter().map(runner_json).collect::<Vec<_>>())))
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct NewRunner {
    name: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "yes")]
    run_untagged: bool,
}

/// `POST /api/v1/runners` `{"name", "tags"?, "run_untagged"?}` → `{"id", "token"}` (token shown once).
pub async fn create_runner(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Json(b): Json<NewRunner>,
) -> AppResult<Response> {
    require_admin(&user)?;
    let tags = ops::parse_tags(b.tags.iter().map(String::as_str));
    let (id, token) = ops::create_runner(&state, &user, &b.name, tags, b.run_untagged).await?.map_err(bad)?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "token": token }))).into_response())
}

#[derive(Deserialize)]
pub struct RunnerPatch {
    tags: Option<Vec<String>>,
    run_untagged: Option<bool>,
    paused: Option<bool>,
}

/// `PATCH /api/v1/runners/{id}` `{"tags"?, "run_untagged"?, "paused"?}` — `tags`
/// replaces the whole list (administrators).
pub async fn patch_runner(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Path(id): Path<i64>,
    Json(b): Json<RunnerPatch>,
) -> AppResult<Json<Value>> {
    require_admin(&user)?;
    let r: Runner = sqlx::query_as(sql!("SELECT {RUNNER_COLS} FROM runners WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let tags = b.tags.map(|t| ops::parse_tags(t.iter().map(String::as_str))).unwrap_or(r.tags);
    let r: Runner = sqlx::query_as(sql!(
        "UPDATE runners SET tags = $2, run_untagged = $3, paused = $4 WHERE id = $1 RETURNING {RUNNER_COLS}"
    ))
    .bind(id)
    .bind(&tags)
    .bind(b.run_untagged.unwrap_or(r.run_untagged))
    .bind(b.paused.unwrap_or(r.paused))
    .fetch_one(&state.db)
    .await?;
    tracing::info!(runner = %r.name, by = %user.email, "runner changed");
    Ok(Json(runner_json(&r)))
}
