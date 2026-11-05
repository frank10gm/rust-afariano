use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use serde::Deserialize;

use super::{RepoNav, load_repo, render};
use crate::access::{self, Role};
use crate::app::Shared;
use crate::auth::{AdminUser, CurrentUser};
use crate::error::{AppError, AppResult, bad};
use crate::models::{RUNNER_COLS, Repo, Runner, User, Variable};
use crate::ssh::parse_public_key;
use crate::util::ago;

fn checked(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.is_empty() && s != "0" && s != "false")
}

// ---------------------------------------------------------------- profile

struct KeyRow {
    id: i64,
    title: String,
    fingerprint: String,
    added: String,
    used: String,
}

struct TokenRow {
    id: i64,
    name: String,
    added: String,
    used: String,
    expires: String,
}

#[derive(Template)]
#[template(path = "profile.html")]
struct ProfilePage {
    user: User,
    keys: Vec<KeyRow>,
    tokens: Vec<TokenRow>,
    new_token: String,
    error: String,
    notice: String,
    has_password: bool,
    password_login: bool,
    ssh_example: String,
    http_example: String,
}

async fn profile_page(state: &Shared, user: User, new_token: String, error: String) -> AppResult<Response> {
    profile_page_with(state, user, new_token, error, String::new()).await
}

async fn profile_page_with(
    state: &Shared,
    user: User,
    new_token: String,
    error: String,
    notice: String,
) -> AppResult<Response> {
    let has_password: bool = sqlx::query_scalar("SELECT password_hash IS NOT NULL FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    type KeyTuple = (i64, String, String, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>);
    let keys: Vec<KeyTuple> = sqlx::query_as(
        "SELECT id, title, fingerprint, created_at, last_used_at FROM ssh_keys WHERE user_id = $1 ORDER BY id",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    type TokTuple = (i64, String, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>);
    let tokens: Vec<TokTuple> = sqlx::query_as(
        "SELECT id, name, created_at, last_used_at, expires_at FROM access_tokens WHERE user_id = $1 ORDER BY id",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    render(ProfilePage {
        keys: keys
            .into_iter()
            .map(|(id, title, fingerprint, c, u)| KeyRow {
                id,
                title,
                fingerprint,
                added: ago(c),
                used: u.map(ago).unwrap_or_else(|| "never".into()),
            })
            .collect(),
        tokens: tokens
            .into_iter()
            .map(|(id, name, c, u, e)| TokenRow {
                id,
                name,
                added: ago(c),
                used: u.map(ago).unwrap_or_else(|| "never".into()),
                expires: e.map(|e| e.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "never".into()),
            })
            .collect(),
        new_token,
        error,
        notice,
        has_password,
        password_login: state.cfg.password_login,
        ssh_example: state.cfg.ssh_clone_url("namespace/repo"),
        http_example: state.cfg.http_clone_url("namespace/repo"),
        user,
    })
}

pub async fn profile(State(state): State<Shared>, CurrentUser(user): CurrentUser) -> AppResult<Response> {
    profile_page(&state, user, String::new(), String::new()).await
}

#[derive(Deserialize)]
pub struct KeyForm {
    title: String,
    key: String,
}

pub async fn add_key(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Form(f): Form<KeyForm>,
) -> AppResult<Response> {
    let (key, fp) = match parse_public_key(&f.key) {
        Ok(k) => k,
        Err(e) => return profile_page(&state, user, String::new(), format!("{e:#}")).await,
    };
    let title = if f.title.trim().is_empty() {
        key.split_whitespace().nth(2).unwrap_or("key").to_string()
    } else {
        f.title.trim().to_string()
    };
    let inserted = sqlx::query("INSERT INTO ssh_keys (user_id, title, public_key, fingerprint) VALUES ($1, $2, $3, $4) ON CONFLICT (fingerprint) DO NOTHING")
        .bind(user.id)
        .bind(&title)
        .bind(&key)
        .bind(&fp)
        .execute(&state.db)
        .await?
        .rows_affected();
    if inserted == 0 {
        return profile_page(&state, user, String::new(), "That key is already registered.".into()).await;
    }
    Ok(Redirect::to("/-/profile").into_response())
}

pub async fn delete_key(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    sqlx::query("DELETE FROM ssh_keys WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/-/profile").into_response())
}

#[derive(Deserialize)]
pub struct TokenForm {
    name: String,
    #[serde(default)]
    expires_days: String,
}

pub async fn add_token(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Form(f): Form<TokenForm>,
) -> AppResult<Response> {
    let days = match f.expires_days.trim() {
        "" => None,
        d => Some(d.parse::<i64>().map_err(|_| bad("expiry must be a number of days"))?),
    };
    match crate::ops::create_token(&state, user.id, &f.name, days).await? {
        Ok(token) => profile_page(&state, user, token, String::new()).await,
        Err(e) => profile_page(&state, user, String::new(), e.into()).await,
    }
}

pub async fn delete_token(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    sqlx::query("DELETE FROM access_tokens WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/-/profile").into_response())
}

#[derive(Deserialize)]
pub struct PasswordChange {
    #[serde(default)]
    current: String,
    password: String,
    password2: String,
}

pub async fn change_password(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Form(f): Form<PasswordChange>,
) -> AppResult<Response> {
    let account = crate::accounts::find_by_email(&state.db, &user.email).await?.ok_or(AppError::NotFound)?;
    if let Some(stored) = account.password_hash
        && !crate::accounts::verify_password(&state, f.current, stored).await
    {
        return profile_page(&state, user, String::new(), "Your current password is not right.".into()).await;
    }
    if let Err(e) = crate::accounts::check_new_password(&f.password, &f.password2) {
        return profile_page(&state, user, String::new(), e.into()).await;
    }
    let hash = crate::accounts::hash_password(&state, f.password).await?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user.id)
        .bind(hash)
        .execute(&state.db)
        .await?;
    tracing::info!(user = %user.email, "password changed");
    profile_page_with(&state, user, String::new(), String::new(), "Password saved.".into()).await
}

// ---------------------------------------------------------------- runners

struct RunnerRow {
    r: Runner,
    online: bool,
    contacted: String,
    tags: String,
}

#[derive(Template)]
#[template(path = "runners.html")]
struct RunnersPage {
    user: User,
    runners: Vec<RunnerRow>,
    new_name: String,
    new_token: String,
    server_url: String,
    error: String,
}

async fn runners_page(
    state: &Shared,
    user: User,
    new_name: String,
    new_token: String,
    error: String,
) -> AppResult<Response> {
    let list: Vec<Runner> =
        sqlx::query_as(sql!("SELECT {RUNNER_COLS} FROM runners ORDER BY id")).fetch_all(&state.db).await?;
    render(RunnersPage {
        user,
        runners: list
            .into_iter()
            .map(|r| RunnerRow {
                online: r.online(),
                contacted: r.contacted_at.map(ago).unwrap_or_else(|| "never".into()),
                tags: r.tags.join(", "),
                r,
            })
            .collect(),
        new_name,
        new_token,
        server_url: state.cfg.public_url.clone(),
        error,
    })
}

pub async fn runners(State(state): State<Shared>, AdminUser(user): AdminUser) -> AppResult<Response> {
    runners_page(&state, user, String::new(), String::new(), String::new()).await
}

#[derive(Deserialize)]
pub struct RunnerForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tags: String,
    run_untagged: Option<String>,
    paused: Option<String>,
}

pub async fn create_runner(
    State(state): State<Shared>,
    AdminUser(user): AdminUser,
    Form(f): Form<RunnerForm>,
) -> AppResult<Response> {
    let tags = crate::ops::parse_tags([f.tags.as_str()]);
    match crate::ops::create_runner(&state, &user, &f.name, tags, checked(&f.run_untagged)).await? {
        Ok((_, token)) => {
            let name = f.name.trim().to_string();
            runners_page(&state, user, name, token, String::new()).await
        }
        Err(e) => runners_page(&state, user, String::new(), String::new(), e.into()).await,
    }
}

pub async fn update_runner(
    State(state): State<Shared>,
    AdminUser(_user): AdminUser,
    Path(id): Path<i64>,
    Form(f): Form<RunnerForm>,
) -> AppResult<Response> {
    sqlx::query("UPDATE runners SET tags = $2, run_untagged = $3, paused = $4 WHERE id = $1")
        .bind(id)
        .bind(crate::ops::parse_tags([f.tags.as_str()]))
        .bind(checked(&f.run_untagged))
        .bind(checked(&f.paused))
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/-/runners").into_response())
}

pub async fn delete_runner(
    State(state): State<Shared>,
    AdminUser(_user): AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    sqlx::query("DELETE FROM runners WHERE id = $1").bind(id).execute(&state.db).await?;
    Ok(Redirect::to("/-/runners").into_response())
}

// ---------------------------------------------------------------- repo settings

struct VarRow {
    v: Variable,
    preview: String,
    /// The preview does not show the value; a Show button fetches it.
    hidden: bool,
}

struct MemberRow {
    user_id: i64,
    email: String,
    name: String,
    initial: String,
    role: Role,
    is_admin: bool,
    disabled: bool,
    is_me: bool,
}

/// Someone who can be added: an active account that is neither a member nor an administrator.
struct Candidate {
    id: i64,
    label: String,
}

/// Settings → Mirror of a pull mirror.
pub(super) struct MirrorView {
    url: String,
    ssh: bool,
    public_key: String,
    host_keys: Vec<String>,
    has_https_token: bool,
    branches: String,
    synced: String,
    checked: String,
    error: String,
    webhook_url: String,
    webhook_secret: String,
    hook: String,
    hook_error: String,
    /// `GitHub`, `Bitbucket`, or empty for other hosts.
    forge: String,
    has_status_token: bool,
    status: String,
    status_error: String,
}

impl MirrorView {
    pub(super) async fn load(state: &Shared, repo: &Repo) -> AppResult<Option<Self>> {
        let (Some(url), Some(m)) = (repo.mirror_url.clone(), crate::mirror::load(state, repo.id).await?) else {
            return Ok(None);
        };
        let up = crate::mirror::Upstream::parse(&url).ok();
        let when = |t: Option<chrono::DateTime<Utc>>| t.map(ago).unwrap_or_else(|| "never".into());
        Ok(Some(Self {
            ssh: up.as_ref().is_some_and(|u| u.transport == crate::mirror::Transport::Ssh),
            forge: up.as_ref().and_then(|u| u.forge()).map(|f| f.name().to_string()).unwrap_or_default(),
            url,
            public_key: m.ssh_public_key,
            host_keys: crate::mirror::host_fingerprints(&m.known_hosts),
            has_https_token: m.https_token.is_some(),
            branches: m.branches,
            synced: when(m.synced_at),
            checked: when(m.checked_at),
            error: m.error.unwrap_or_default(),
            webhook_url: crate::mirror::webhook_url(state, repo),
            webhook_secret: state.cipher.decrypt(&m.webhook_secret).unwrap_or_else(|_| "(cannot decrypt)".into()),
            hook: when(m.hook_at),
            hook_error: m.hook_error.unwrap_or_default(),
            has_status_token: m.status_token.is_some(),
            status: when(m.status_at),
            status_error: m.status_error.unwrap_or_default(),
        }))
    }
}

#[derive(Template)]
#[template(path = "repo_settings.html")]
struct RepoSettingsPage {
    user: User,
    nav: RepoNav,
    repo: Repo,
    branches: Vec<String>,
    variables: Vec<VarRow>,
    members: Vec<MemberRow>,
    candidates: Vec<Candidate>,
    roles: [Role; 3],
    mirror: Option<MirrorView>,
    error: String,
    notice: String,
}

async fn settings_page(state: &Shared, user: User, repo: Repo, error: String, notice: String) -> AppResult<Response> {
    let members: Vec<(i64, String, String, bool, bool, String)> = sqlx::query_as(
        "SELECT u.id, u.email, u.name, u.is_admin, u.disabled, m.role FROM repo_members m JOIN users u ON u.id = m.user_id \
         WHERE m.repo_id = $1 ORDER BY lower(COALESCE(NULLIF(u.name, ''), u.email))",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;
    let members = members
        .into_iter()
        .map(|(user_id, email, name, is_admin, disabled, role)| MemberRow {
            initial: super::initial(if name.is_empty() { &email } else { &name }),
            role: Role::parse(&role).unwrap_or(Role::Read),
            is_me: user_id == user.id,
            user_id,
            email,
            name,
            is_admin,
            disabled,
        })
        .collect();
    let candidates: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, email, name FROM users WHERE NOT disabled AND NOT is_admin \
         AND id NOT IN (SELECT user_id FROM repo_members WHERE repo_id = $1) ORDER BY lower(email)",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;
    let candidates = candidates
        .into_iter()
        .map(|(id, email, name)| Candidate {
            id,
            label: if name.is_empty() { email } else { format!("{name} ({email})") },
        })
        .collect();
    let vars: Vec<Variable> = sqlx::query_as(
        "SELECT id, repo_id, key, value, file, protected, masked, environment_scope FROM variables WHERE repo_id = $1 ORDER BY key, environment_scope",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;
    let refs = state.git(repo.id).refs().await?;
    let mirror = MirrorView::load(state, &repo).await?;
    render(RepoSettingsPage {
        user,
        mirror,
        nav: RepoNav::new(&repo, Role::Maintain, "settings"),
        members,
        candidates,
        roles: Role::ALL,
        branches: refs.iter().filter(|r| !r.is_tag()).map(|r| r.short().to_string()).collect(),
        variables: vars
            .into_iter()
            .map(|mut v| {
                let plain = state.cipher.decrypt(&v.value).unwrap_or_else(|_| "(cannot decrypt)".into());
                let hidden = v.masked || v.file || plain.chars().count() > 40;
                let preview = if hidden { "••••••".to_string() } else { plain };
                v.value.clear();
                VarRow { v, preview, hidden }
            })
            .collect(),
        repo,
        error,
        notice,
    })
}

pub async fn repo_settings(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    settings_page(&state, user, repo, String::new(), String::new()).await
}

#[derive(Deserialize)]
pub struct RepoForm {
    #[serde(default)]
    description: String,
    default_branch: String,
    #[serde(default)]
    protected_branches: String,
    pipelines_on_push: Option<String>,
    prevent_outdated_deploys: Option<String>,
    list_shows_variables: Option<String>,
}

pub async fn update_repo(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Form(f): Form<RepoForm>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let settings = crate::ops::RepoSettings {
        description: f.description,
        default_branch: f.default_branch,
        protected_branches: f.protected_branches.split(',').map(str::to_string).collect(),
        pipelines_on_push: checked(&f.pipelines_on_push),
        prevent_outdated_deploys: checked(&f.prevent_outdated_deploys),
        list_shows_variables: checked(&f.list_shows_variables),
    };
    if let Err(e) = crate::ops::update_repo(&state, &repo, settings).await? {
        return settings_page(&state, user, repo, e.into(), String::new()).await;
    }
    let repo = state.repo_by_id(repo.id).await?;
    settings_page(&state, user, repo, String::new(), "Settings saved.".into()).await
}

#[derive(Deserialize)]
pub struct VariableForm {
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    environment_scope: String,
    file: Option<String>,
    protected: Option<String>,
    masked: Option<String>,
}

pub async fn save_variable(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Form(f): Form<VariableForm>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let input = crate::ops::VariableInput {
        key: f.key,
        value: f.value,
        file: checked(&f.file),
        protected: checked(&f.protected),
        masked: checked(&f.masked),
        environment_scope: f.environment_scope,
    };
    match crate::ops::upsert_variable(&state, repo.id, input).await? {
        Ok(key) => settings_page(&state, user, repo, String::new(), format!("Variable {key} saved.")).await,
        Err(e) => settings_page(&state, user, repo, e.into(), String::new()).await,
    }
}

pub async fn delete_variable(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, id)): Path<(String, String, i64)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    sqlx::query("DELETE FROM variables WHERE id = $1 AND repo_id = $2")
        .bind(id)
        .bind(repo.id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to(&format!("/{}/-/settings#variables", repo.path())).into_response())
}

/// A variable's value as plain text, for maintainers. POST so it passes the
/// same-origin check and is never prefetched; never cached; logged.
pub async fn variable_value(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, id)): Path<(String, String, i64)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let (key, sealed): (String, String) =
        sqlx::query_as("SELECT key, value FROM variables WHERE id = $1 AND repo_id = $2")
            .bind(id)
            .bind(repo.id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let value = state.cipher.decrypt(&sealed).map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt {key}: {e}")))?;
    tracing::info!(repo = %repo.path(), key = %key, user = %user.email, "variable value shown");
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        value,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct MemberForm {
    user_id: i64,
    role: String,
}

/// Add a member, or change a member's role.
pub async fn save_member(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Form(f): Form<MemberForm>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let role = Role::parse(&f.role).ok_or_else(|| bad("unknown role"))?;
    let who: Option<(String, String)> =
        sqlx::query_as("SELECT email, name FROM users WHERE id = $1").bind(f.user_id).fetch_optional(&state.db).await?;
    let Some((email, who_name)) = who else {
        return settings_page(&state, user, repo, "That account no longer exists.".into(), String::new()).await;
    };
    crate::ops::set_member(&state, repo.id, f.user_id, role).await?;
    tracing::info!(repo = %repo.path(), member = %email, role = role.as_str(), by = %user.email, "member saved");
    let display = if who_name.is_empty() { email } else { who_name };
    after_member_change(&state, user, repo, format!("{display} now has {} access.", role.label().to_lowercase())).await
}

pub async fn delete_member(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, member)): Path<(String, String, i64)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let email: Option<String> = sqlx::query_scalar(
        "DELETE FROM repo_members m USING users u WHERE m.repo_id = $1 AND m.user_id = $2 AND u.id = m.user_id \
         RETURNING u.email",
    )
    .bind(repo.id)
    .bind(member)
    .fetch_optional(&state.db)
    .await?;
    let Some(email) = email else {
        return settings_page(&state, user, repo, String::new(), String::new()).await;
    };
    tracing::info!(repo = %repo.path(), member = %email, by = %user.email, "member removed");
    after_member_change(&state, user, repo, format!("{email} was removed.")).await
}

/// Someone who lowered or removed their own access cannot see this page any more.
async fn after_member_change(state: &Shared, user: User, repo: Repo, notice: String) -> AppResult<Response> {
    match access::role(state, &user, repo.id).await? {
        Some(Role::Maintain) => settings_page(state, user, repo, String::new(), notice).await,
        Some(_) => Ok(Redirect::to(&format!("/{}", repo.path())).into_response()),
        None => Ok(Redirect::to("/").into_response()),
    }
}

// ---------------------------------------------------------------- mirror

#[derive(Deserialize)]
pub struct MirrorForm {
    #[serde(default)]
    url: String,
    branches: Option<String>,
    #[serde(default)]
    https_token: String,
    clear_https_token: Option<String>,
    #[serde(default)]
    status_token: String,
    clear_status_token: Option<String>,
}

/// A secret field: empty keeps the stored value, the Remove box clears it.
fn secret_field(value: &str, clear: &Option<String>) -> Option<String> {
    if checked(clear) {
        Some(String::new())
    } else if value.trim().is_empty() {
        None
    } else {
        Some(value.trim().to_string())
    }
}

/// Starts mirroring (administrators: the URL decides which server this one
/// talks to) or changes a mirror's settings (maintainers).
pub async fn save_mirror(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Form(f): Form<MirrorForm>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    let url = if f.url.trim().is_empty() { repo.mirror_url.clone().unwrap_or_default() } else { f.url.clone() };
    if repo.mirror_url.as_deref() != Some(url.trim()) && !user.is_admin {
        return Err(AppError::Forbidden);
    }
    let starting = !repo.is_mirror();
    let input = crate::mirror::MirrorInput {
        url,
        branches: f.branches,
        https_token: secret_field(&f.https_token, &f.clear_https_token),
        status_token: secret_field(&f.status_token, &f.clear_status_token),
    };
    if let Err(e) = crate::mirror::configure(&state, &repo, input).await? {
        return settings_page(&state, user, repo, e.into(), String::new()).await;
    }
    tracing::info!(repo = %repo.path(), by = %user.email, "mirror settings saved");
    let repo = state.repo_by_id(repo.id).await?;
    crate::mirror::request_sync(&state, repo.id);
    let notice = if starting {
        "This repository is now a mirror. Give its key (or token) read access on the upstream, then Sync now."
    } else {
        "Mirror settings saved."
    };
    settings_page(&state, user, repo, String::new(), notice.into()).await
}

#[derive(Deserialize)]
pub struct SyncForm {
    #[serde(default)]
    back: String,
}

/// Fetches the upstream now (write access, like running a pipeline).
pub async fn sync_mirror(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Form(f): Form<SyncForm>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Write).await?;
    if !repo.is_mirror() {
        return Err(bad("This repository is not a mirror."));
    }
    let result = crate::mirror::sync_for_pipeline(&state, &repo).await;
    if f.back != "settings" || role < Role::Maintain {
        // The overview shows the outcome (last sync, or its error).
        return Ok(Redirect::to(&format!("/{}", repo.path())).into_response());
    }
    let repo = state.repo_by_id(repo.id).await?;
    match result {
        Ok(synced) => {
            let notice = match synced.changed {
                0 => "Up to date with the upstream: nothing new.".to_string(),
                1 => "Up to date with the upstream: 1 branch or tag changed.".to_string(),
                n => format!("Up to date with the upstream: {n} branches and tags changed."),
            };
            settings_page(&state, user, repo, String::new(), notice).await
        }
        Err(e) => settings_page(&state, user, repo, e, String::new()).await,
    }
}

pub async fn forget_mirror_host(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    crate::mirror::forget_host_key(&state, repo.id).await?;
    tracing::warn!(repo = %repo.path(), by = %user.email, "mirror host key forgotten");
    let notice = "Host key forgotten: the next sync pins the key the upstream presents.";
    settings_page(&state, user, repo, String::new(), notice.into()).await
}

pub async fn new_mirror_secret(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    if !repo.is_mirror() {
        return Err(bad("This repository is not a mirror."));
    }
    crate::mirror::new_webhook_secret(&state, repo.id).await?;
    let notice = "New webhook secret: put it on the upstream's webhook, the old one no longer works.";
    settings_page(&state, user, repo, String::new(), notice.into()).await
}

/// Turns a mirror into an ordinary repository that accepts pushes.
pub async fn stop_mirror(
    State(state): State<Shared>,
    AdminUser(user): AdminUser,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    crate::mirror::stop(&state, &repo).await?;
    tracing::warn!(repo = %repo.path(), by = %user.email, "mirroring stopped");
    let repo = state.repo_by_id(repo.id).await?;
    let notice = "No longer a mirror: it accepts pushes again. Remove its key from the upstream.";
    settings_page(&state, user, repo, String::new(), notice.into()).await
}

#[derive(Deserialize)]
pub struct DeleteForm {
    confirm: String,
}

pub async fn delete_repo(
    State(state): State<Shared>,
    AdminUser(user): AdminUser,
    Path((ns, name)): Path<(String, String)>,
    Form(f): Form<DeleteForm>,
) -> AppResult<Response> {
    let (repo, _) = load_repo(&state, &user, &ns, &name, Role::Maintain).await?;
    if f.confirm.trim() != repo.path() {
        return settings_page(&state, user, repo, "Type the full path to confirm deletion.".into(), String::new())
            .await;
    }
    let job_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM jobs WHERE repo_id = $1").bind(repo.id).fetch_all(&state.db).await?;
    sqlx::query("DELETE FROM repos WHERE id = $1").bind(repo.id).execute(&state.db).await?;
    for id in job_ids {
        state.logs.delete(id).await;
    }
    let dir = crate::git::repo_dir(&state.cfg.data_dir, repo.id);
    if dir.starts_with(state.cfg.data_dir.join("repos")) {
        tokio::fs::remove_dir_all(&dir).await.map_err(|e| AppError::Internal(e.into()))?;
    }
    tracing::warn!(repo = %repo.path(), user = %user.email, "repository deleted");
    Ok(Redirect::to("/").into_response())
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::ProfilePage;
    use crate::models::User;

    /// Regression: the password section once leaked into every block of the
    /// template (page title, sidebar link class, breadcrumbs).
    #[test]
    fn profile_page_renders_each_section_once() {
        for has_password in [true, false] {
            let html = ProfilePage {
                user: User { id: 1, email: "ana@example.com".into(), name: String::new(), is_admin: true },
                keys: Vec::new(),
                tokens: Vec::new(),
                new_token: String::new(),
                error: String::new(),
                notice: String::new(),
                has_password,
                password_login: true,
                ssh_example: "ssh://git@host:2222/ns/repo.git".into(),
                http_example: "https://host/ns/repo.git".into(),
            }
            .render()
            .unwrap();
            assert_eq!(html.matches("<h2>Password</h2>").count(), 1);
            assert!(html.contains("<title>Your account · rust-afariano</title>"));
            assert!(html.contains(r#"<a href="/-/profile" class="on">"#));
            assert_eq!(html.matches("action=\"/-/profile/password\"").count(), 1);
        }
    }
}
