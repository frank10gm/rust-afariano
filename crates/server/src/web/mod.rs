//! Server-rendered pages (askama), routes, and small view helpers.

mod admin;
mod ci;
mod repo;
mod settings;

pub(crate) use ci::strip_ansi;

use askama::Template;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post, put};

use crate::access::{self, Role};
use crate::app::Shared;
use crate::auth;
use crate::error::{AppError, AppResult, bad};
use crate::models::{Job, Pipeline, PipelineVar, Repo, User};
use crate::util::{ago, duration, short_sha};
use crate::{api, git_http, runner_api};

/// Appended to the URLs of assets that change between releases (`?v=…`), so
/// browsers fetch them again after an upgrade instead of using a cached copy.
pub const ASSET_V: &str = env!("CARGO_PKG_VERSION");

/// The project's source repository (`repository` in Cargo.toml), linked in the sidebar.
pub const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

pub fn router(state: Shared) -> Router {
    let pages = Router::new()
        .route("/", get(repo::home))
        .route("/-/new", get(repo::new_form).post(repo::create))
        .route("/-/profile", get(settings::profile))
        .route("/-/profile/keys", post(settings::add_key))
        .route("/-/profile/keys/{id}/delete", post(settings::delete_key))
        .route("/-/profile/tokens", post(settings::add_token))
        .route("/-/profile/tokens/{id}/delete", post(settings::delete_token))
        .route("/-/profile/password", post(settings::change_password))
        .route("/-/admin", get(admin::redirect_users))
        .route("/-/admin/users", get(admin::users).post(admin::create_user))
        .route("/-/admin/users/{id}", post(admin::update_user))
        .route("/-/setup", get(auth::setup_form).post(auth::setup_submit))
        .route("/auth/password", post(auth::password_login))
        .route("/-/runners", get(settings::runners).post(settings::create_runner))
        .route("/-/runners/{id}", post(settings::update_runner))
        .route("/-/runners/{id}/delete", post(settings::delete_runner))
        .route("/auth/logout", post(auth::logout))
        .route("/{ns}/{name}", get(repo::overview))
        .route("/{ns}/{name}/-/tree/{*rest}", get(repo::tree))
        .route("/{ns}/{name}/-/blob/{*rest}", get(repo::blob))
        .route("/{ns}/{name}/-/raw/{*rest}", get(repo::raw))
        .route("/{ns}/{name}/-/commits", get(repo::commits_default))
        .route("/{ns}/{name}/-/commits/{*rest}", get(repo::commits))
        .route("/{ns}/{name}/-/commit/{sha}", get(repo::commit))
        .route("/{ns}/{name}/-/pipelines", get(ci::pipelines))
        .route("/{ns}/{name}/-/pipelines/new", get(ci::new_pipeline_form).post(ci::run_pipeline))
        .route("/{ns}/{name}/-/pipelines/{iid}", get(ci::pipeline))
        .route("/{ns}/{name}/-/pipelines/{iid}/{action}", post(ci::pipeline_action))
        .route("/{ns}/{name}/-/jobs/{id}", get(ci::job))
        .route("/{ns}/{name}/-/jobs/{id}/raw", get(ci::job_raw))
        .route("/{ns}/{name}/-/jobs/{id}/stream", get(ci::job_stream))
        .route("/{ns}/{name}/-/jobs/{id}/{action}", post(ci::job_action))
        .route("/{ns}/{name}/-/settings", get(settings::repo_settings).post(settings::update_repo))
        .route("/{ns}/{name}/-/settings/variables", post(settings::save_variable))
        .route("/{ns}/{name}/-/settings/variables/{id}/delete", post(settings::delete_variable))
        .route("/{ns}/{name}/-/settings/variables/{id}/value", post(settings::variable_value))
        .route("/{ns}/{name}/-/settings/delete", post(settings::delete_repo))
        .route("/{ns}/{name}/-/settings/members", post(settings::save_member))
        .route("/{ns}/{name}/-/settings/members/{user_id}/delete", post(settings::delete_member))
        .route("/{ns}/{name}/-/settings/mirror", post(settings::save_mirror))
        .route("/{ns}/{name}/-/settings/mirror/sync", post(settings::sync_mirror))
        .route("/{ns}/{name}/-/settings/mirror/forget-host", post(settings::forget_mirror_host))
        .route("/{ns}/{name}/-/settings/mirror/secret", post(settings::new_mirror_secret))
        .route("/{ns}/{name}/-/settings/mirror/delete", post(settings::stop_mirror))
        .layer(middleware::from_fn_with_state(state.clone(), auth::same_origin));

    let git = Router::new()
        .route("/{ns}/{name}/info/refs", get(git_http::info_refs))
        .route("/{ns}/{name}/git-upload-pack", post(git_http::upload_pack))
        .route("/{ns}/{name}/git-receive-pack", post(git_http::receive_pack))
        .layer(DefaultBodyLimit::disable());

    let api = Router::new()
        .route("/api/v1/runner/verify", post(runner_api::verify))
        .route("/api/v1/runner/jobs/request", post(runner_api::request_job))
        .route("/api/v1/runner/jobs/{id}/log", put(runner_api::put_log))
        .route("/api/v1/runner/jobs/{id}/finish", post(runner_api::finish_job))
        .route("/api/v1/repos", post(api::create_repo))
        .route("/api/v1/repos/{ns}/{name}", get(api::get_repo).patch(api::patch_repo))
        .route("/api/v1/repos/{ns}/{name}/mirror", get(api::get_mirror).put(api::put_mirror).delete(api::delete_mirror))
        .route("/api/v1/repos/{ns}/{name}/mirror/sync", post(api::sync_mirror))
        .route("/api/v1/repos/{ns}/{name}/mirror/secret", post(api::new_mirror_secret))
        .route("/api/v1/repos/{ns}/{name}/members", get(api::list_members))
        .route("/api/v1/repos/{ns}/{name}/members/{user}", put(api::put_member).delete(api::delete_member))
        .route("/api/v1/repos/{ns}/{name}/variables", get(api::list_variables))
        .route("/api/v1/repos/{ns}/{name}/variables/{key}", put(api::put_variable).delete(api::delete_variable))
        .route("/api/v1/repos/{ns}/{name}/pipelines", post(runner_api::trigger_pipeline))
        .route("/api/v1/repos/{ns}/{name}/pipelines/{iid}", get(api::get_pipeline))
        .route("/api/v1/repos/{ns}/{name}/pipelines/{iid}/{action}", post(api::pipeline_action))
        .route("/api/v1/repos/{ns}/{name}/jobs/{id}/log", get(api::job_log))
        .route("/api/v1/repos/{ns}/{name}/jobs/{id}/{action}", post(api::job_action))
        .route("/api/v1/users", get(api::list_users).post(api::create_user))
        .route("/api/v1/users/{user}/tokens", post(api::create_user_token))
        .route("/api/v1/runners", get(api::list_runners).post(api::create_runner))
        .route("/api/v1/runners/{id}", patch(api::patch_runner))
        .layer(DefaultBodyLimit::max(2 << 20))
        .layer(middleware::map_response(crate::error::json_errors));

    // Webhook payloads can be larger than API bodies; only their signature is checked.
    let hooks = Router::new()
        .route("/api/v1/repos/{ns}/{name}/mirror/hook", post(api::mirror_hook))
        .layer(DefaultBodyLimit::max(8 << 20))
        .layer(middleware::map_response(crate::error::json_errors));

    Router::new()
        .merge(pages)
        .merge(git)
        .merge(api)
        .merge(hooks)
        .route("/auth/login", get(auth::login))
        .route("/auth/oidc", get(auth::oidc_start))
        .route("/auth/oidc/callback", get(auth::oidc_callback))
        .route("/auth/rich", get(auth::rich_start))
        .route("/auth/callback", get(auth::rich_callback))
        .route("/signed-out", get(signed_out))
        .route("/-/static/{file}", get(static_file))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(|| async { AppError::NotFound })
        .layer(middleware::map_response(security_headers))
        .with_state(state)
}

async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(
        "default-src 'self'; img-src 'self' https: data:; style-src 'self' 'unsafe-inline'; script-src 'self'; \
         object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    ));
    resp
}

async fn static_file(Path(file): Path<String>) -> Response {
    let (body, ctype, max_age): (&'static [u8], &str, u32) = match file.as_str() {
        "app.css" => (include_bytes!("../../static/app.css"), "text/css; charset=utf-8", 300),
        "app.js" => (include_bytes!("../../static/app.js"), "text/javascript; charset=utf-8", 300),
        "icons.svg" => (include_bytes!("../../static/icons.svg"), "image/svg+xml", 3600),
        "logo.png" => (include_bytes!("../../static/logo.png"), "image/png", 86400),
        "favicon.png" => (include_bytes!("../../static/favicon.png"), "image/png", 86400),
        "plex-sans-400.woff2" => (include_bytes!("../../static/fonts/plex-sans-400.woff2"), "font/woff2", 2_592_000),
        "plex-sans-500.woff2" => (include_bytes!("../../static/fonts/plex-sans-500.woff2"), "font/woff2", 2_592_000),
        "plex-sans-600.woff2" => (include_bytes!("../../static/fonts/plex-sans-600.woff2"), "font/woff2", 2_592_000),
        "plex-mono-400.woff2" => (include_bytes!("../../static/fonts/plex-mono-400.woff2"), "font/woff2", 2_592_000),
        "plex-mono-500.woff2" => (include_bytes!("../../static/fonts/plex-mono-500.woff2"), "font/woff2", 2_592_000),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let cache = format!("public, max-age={max_age}");
    ([(header::CONTENT_TYPE, ctype.to_string()), (header::CACHE_CONTROL, cache)], body).into_response()
}

#[derive(Template)]
#[template(path = "signed_out.html")]
struct SignedOut;

async fn signed_out() -> AppResult<Html<String>> {
    Ok(Html(SignedOut.render()?))
}

pub fn render(t: impl Template) -> AppResult<Response> {
    Ok(Html(t.render()?).into_response())
}

/// Sidebar section and breadcrumbs of every repository page.
pub struct RepoNav {
    pub path: String,
    pub ns: String,
    pub name: String,
    pub description: String,
    pub hue: u16,
    pub initial: String,
    pub tab: &'static str,
    /// The viewer's role, deciding which tabs and buttons show.
    pub role: Role,
    /// For a pull mirror: its upstream (`bitbucket.org/molecle/davegantt`); else empty.
    pub mirror: String,
    /// The upstream's web page, if known.
    pub mirror_web: String,
}

impl RepoNav {
    pub fn new(repo: &Repo, role: Role, tab: &'static str) -> Self {
        Self {
            role,
            path: repo.path(),
            ns: repo.namespace.clone(),
            name: repo.name.clone(),
            description: repo.description.clone(),
            hue: hue(&repo.path()),
            initial: initial(&repo.name),
            tab,
            mirror: repo.mirror_url.as_deref().map(mirror_display).unwrap_or_default(),
            mirror_web: repo
                .mirror_url
                .as_deref()
                .and_then(|u| crate::mirror::Upstream::parse(u).ok())
                .and_then(|u| u.web_url())
                .unwrap_or_default(),
        }
    }

    pub fn is_mirror(&self) -> bool {
        !self.mirror.is_empty()
    }

    pub fn can_write(&self) -> bool {
        self.role >= Role::Write
    }

    pub fn can_maintain(&self) -> bool {
        self.role >= Role::Maintain
    }
}

pub fn mirror_display(url: &str) -> String {
    crate::mirror::Upstream::parse(url).map(|u| u.display()).unwrap_or_else(|_| url.to_string())
}

/// A stable hue per name, for the coloured avatar tiles (like GitLab's identicons).
pub fn hue(s: &str) -> u16 {
    let h = s.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    (h % 360) as u16
}

pub fn initial(s: &str) -> String {
    s.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into())
}

/// Status word shown next to the icon, in GitLab's vocabulary.
pub fn status_label(status: &str, pipeline: bool) -> &'static str {
    match status {
        "success" => "Passed",
        "failed" => "Failed",
        "warning" => "Passed with warnings",
        "running" => "Running",
        "pending" => "Pending",
        "manual" if pipeline => "Blocked",
        "manual" => "Manual",
        "canceled" => "Canceled",
        "skipped" => "Skipped",
        "created" => "Created",
        _ => "Unknown",
    }
}

/// The repository at `ns/name` and the user's role in it, if that role is at least `need`.
pub async fn load_repo(state: &Shared, user: &User, ns: &str, name: &str, need: Role) -> AppResult<(Repo, Role)> {
    access::repo(state, user, ns, name, need).await
}

/// One stage of a pipeline, summarised as a single status (the list's mini graph).
pub struct StageDot {
    pub name: String,
    pub status: String,
}

impl StageDot {
    pub fn label(&self) -> &'static str {
        status_label(&self.status, false)
    }
}

pub struct PipelineRow {
    pub id: i64,
    pub iid: i32,
    pub url: String,
    pub repo_path: String,
    pub status: String,
    pub ref_name: String,
    pub ref_is_tag: bool,
    pub sha_short: String,
    pub title: String,
    pub source: String,
    pub created_ago: String,
    pub duration: String,
    pub by: String,
    pub stages: Vec<StageDot>,
    /// Variables the run was started with, in the order given.
    pub vars: Vec<RowVar>,
    /// List mode "variables": they replace the title (see `for_repo`).
    pub vars_mode: bool,
    /// Show the ref chip; in variables mode only when it is not the default branch.
    pub show_ref: bool,
}

/// One run variable as the list shows it. A hidden value (secret, or from a pipeline
/// stored before the flag existed) is left out of the page entirely.
pub struct RowVar {
    pub key: String,
    pub value: String,
    pub hidden: bool,
}

impl RowVar {
    fn new(v: &PipelineVar) -> Self {
        let hidden = !v.value_listed();
        Self { key: v.key.clone(), value: if hidden { String::new() } else { v.value.clone() }, hidden }
    }
}

impl PipelineRow {
    pub fn new(p: &Pipeline, repo_path: &str, by: Option<&str>) -> Self {
        let duration = match (p.started_at, p.finished_at) {
            (Some(s), Some(f)) => duration((f - s).num_seconds()),
            // Only a running pipeline has a live clock; one waiting on a manual job does not.
            (Some(s), None) if p.status == "running" => duration((chrono::Utc::now() - s).num_seconds()),
            _ => String::new(),
        };
        Self {
            id: p.id,
            iid: p.iid,
            url: format!("/{repo_path}/-/pipelines/{}", p.iid),
            repo_path: repo_path.to_string(),
            status: p.status.clone(),
            ref_name: p.ref_name.clone(),
            ref_is_tag: p.ref_is_tag,
            sha_short: short_sha(&p.sha).to_string(),
            title: p.commit_title.clone(),
            source: p.source.clone(),
            created_ago: ago(p.created_at),
            duration,
            by: by.unwrap_or("").to_string(),
            stages: Vec::new(),
            vars: p.variables.0.iter().map(RowVar::new).collect(),
            vars_mode: false,
            show_ref: true,
        }
    }

    /// Applies the repository's list setting: with variables to show, they replace the
    /// commit title (the hash stays), and the ref is shown only when it is not the default branch.
    pub fn for_repo(mut self, repo: &Repo) -> Self {
        if repo.list_shows_variables && !self.vars.is_empty() {
            self.vars_mode = true;
            self.show_ref = self.ref_is_tag || self.ref_name != repo.default_branch;
        }
        self
    }

    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "created" | "pending" | "running")
    }

    pub fn label(&self) -> &'static str {
        status_label(&self.status, true)
    }

    /// "Pushed by Ana", "Started by Ana", "Started through the API".
    pub fn trigger(&self) -> String {
        let who = if self.by.is_empty() { String::new() } else { format!(" by {}", self.by) };
        match self.source.as_str() {
            "push" => format!("Pushed{who}"),
            "web" => format!("Started{who}"),
            "api" => format!("Started through the API{who}"),
            other => other.to_string(),
        }
    }
}

/// Mini-graph data for a page of pipelines: one status per stage, in order.
pub async fn attach_stages(state: &Shared, rows: &mut [PipelineRow]) -> sqlx::Result<()> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let jobs: Vec<(i64, i32, String, String, bool)> = sqlx::query_as(
        "SELECT pipeline_id, stage_idx, stage, status, allow_failure FROM jobs \
         WHERE pipeline_id = ANY($1) AND NOT retried ORDER BY pipeline_id, stage_idx, id",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    for row in rows.iter_mut() {
        let mine: Vec<&(i64, i32, String, String, bool)> = jobs.iter().filter(|j| j.0 == row.id).collect();
        let mut idxs: Vec<i32> = mine.iter().map(|j| j.1).collect();
        idxs.dedup();
        for idx in idxs {
            let stage: Vec<&&(i64, i32, String, String, bool)> = mine.iter().filter(|j| j.1 == idx).collect();
            let has = |s: &str| stage.iter().any(|j| j.3 == s);
            let status = if has("running") {
                "running"
            } else if has("pending") {
                "pending"
            } else if stage.iter().any(|j| j.3 == "failed" && !j.4) {
                "failed"
            } else if has("canceled") {
                "canceled"
            } else if has("created") {
                "created"
            } else if stage.iter().any(|j| j.3 == "manual" && !j.4) {
                "manual"
            } else if stage.iter().all(|j| j.3 == "skipped") {
                "skipped"
            } else if has("failed") {
                "warning"
            } else if stage.iter().all(|j| j.3 == "manual" || j.3 == "skipped") {
                "manual"
            } else {
                "success"
            };
            row.stages.push(StageDot { name: stage[0].2.clone(), status: status.into() });
        }
    }
    Ok(())
}

pub struct JobRow {
    pub url: String,
    pub name: String,
    pub stage: String,
    pub status: String,
    /// What the icon shows: `warning` for a failure that is allowed.
    pub vis: String,
    pub allow_failure: bool,
    pub duration: String,
    pub failure_reason: String,
    pub environment: String,
    pub can_play: bool,
    pub can_retry: bool,
    pub can_cancel: bool,
}

impl JobRow {
    pub fn new(j: &Job, repo_path: &str) -> Self {
        Self {
            url: format!("/{repo_path}/-/jobs/{}", j.id),
            name: j.name.clone(),
            stage: j.stage.clone(),
            status: j.status.clone(),
            vis: if j.status == "failed" && j.allow_failure { "warning".into() } else { j.status.clone() },
            allow_failure: j.allow_failure,
            duration: j.duration_secs().map(duration).unwrap_or_default(),
            failure_reason: j.failure_reason.clone().unwrap_or_default().replace('_', " "),
            environment: j.environment.clone().unwrap_or_default(),
            can_play: j.status == "manual",
            can_retry: matches!(j.status.as_str(), "success" | "failed" | "canceled"),
            can_cancel: matches!(j.status.as_str(), "created" | "pending" | "running" | "manual")
                && !j.cancel_requested,
        }
    }

    pub fn label(&self) -> &'static str {
        if self.vis == "warning" { "Failed (allowed)" } else { status_label(&self.status, false) }
    }
}

pub async fn user_names(
    state: &Shared,
    ids: impl Iterator<Item = i64>,
) -> sqlx::Result<std::collections::HashMap<i64, String>> {
    let ids: Vec<i64> = ids.collect();
    if ids.is_empty() {
        return Ok(Default::default());
    }
    let rows: Vec<User> =
        sqlx::query_as("SELECT id, email, name FROM users WHERE id = ANY($1)").bind(&ids).fetch_all(&state.db).await?;
    Ok(rows.into_iter().map(|u| (u.id, u.display().to_string())).collect())
}

/// Validate and trim pipeline variables from a form or the API.
pub fn clean_vars(vars: Vec<PipelineVar>) -> AppResult<Vec<PipelineVar>> {
    let mut out = Vec::new();
    for v in vars {
        let key = v.key.trim().to_string();
        if key.is_empty() {
            continue;
        }
        if !valid_var_key(&key) {
            return Err(bad(format!("invalid variable name `{key}` (letters, digits, _)")));
        }
        let secret = v.is_secret();
        // As for masked project variables: a shorter value would not be masked in job logs.
        if secret && (v.value.trim().len() < 8 || v.value.trim().contains('\n')) {
            return Err(bad(format!("`{key}`: a secret value must be a single line of at least 8 characters")));
        }
        out.push(PipelineVar { key, value: v.value, secret: Some(secret) });
    }
    Ok(out)
}

pub fn valid_var_key(k: &str) -> bool {
    let mut c = k.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && k.len() <= 255
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// README / Markdown → HTML. Raw HTML is shown as text and `javascript:` links
/// are dropped; relative links point into the repository at `base` (`/ns/name/-/blob/ref/dir`).
pub fn markdown(src: &str, blob_base: &str, raw_base: &str) -> String {
    use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};
    fn fix(url: CowStr<'_>, base: &str) -> CowStr<'static> {
        let u = url.trim();
        let lower = u.to_ascii_lowercase();
        if lower.starts_with("javascript:") || lower.starts_with("data:") || lower.starts_with("vbscript:") {
            return CowStr::Borrowed("#");
        }
        if u.contains("://") || u.starts_with('#') || u.starts_with('/') || lower.starts_with("mailto:") {
            return CowStr::from(u.to_string());
        }
        CowStr::from(format!("{base}/{}", u.trim_start_matches("./")))
    }
    let parser =
        Parser::new_ext(src, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS).map(
            |ev| match ev {
                Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
                Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
                    Event::Start(Tag::Link { link_type, dest_url: fix(dest_url, blob_base), title, id })
                }
                Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                    Event::Start(Tag::Image { link_type, dest_url: fix(dest_url, raw_base), title, id })
                }
                other => other,
            },
        );
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_sanitized() {
        let html = markdown(
            "<script>x</script>\n\n[a](javascript:alert(1)) ![i](img.png) [d](docs/x.md)",
            "/n/r/-/blob/main",
            "/n/r/-/raw/main",
        );
        assert!(!html.contains("<script>"));
        assert!(html.contains("href=\"#\""));
        assert!(html.contains("src=\"/n/r/-/raw/main/img.png\""));
        assert!(html.contains("href=\"/n/r/-/blob/main/docs/x.md\""));
    }

    fn var(key: &str, value: &str, secret: Option<bool>) -> PipelineVar {
        PipelineVar { key: key.into(), value: value.into(), secret }
    }

    #[test]
    fn clean_vars_sets_the_secret_flag() {
        let v = clean_vars(vec![var("A", "1", None), var("TOKEN", "s3cr3t-value", Some(true))]).unwrap();
        assert_eq!(v[0].secret, Some(false), "API calls without the flag: not secret");
        assert!(v[1].is_secret());
        // Shorter or multi-line values could not be masked in job logs.
        assert!(clean_vars(vec![var("TOKEN", "short", Some(true))]).is_err());
        assert!(clean_vars(vec![var("TOKEN", "line one\nline two", Some(true))]).is_err());
        let stored = serde_json::to_string(&v).unwrap();
        assert!(stored.contains(r#""secret":false"#) && stored.contains(r#""secret":true"#));
    }

    #[test]
    fn list_hides_secret_and_older_values() {
        let old: Vec<PipelineVar> = serde_json::from_str(r#"[{"key":"A","value":"1"}]"#).unwrap();
        assert_eq!(old[0].secret, None);
        let given = [old[0].clone(), var("B", "2", Some(false)), var("C", "s3cr3t-value", Some(true))];
        let rows: Vec<RowVar> = given.iter().map(RowVar::new).collect();
        assert!(rows[0].hidden && rows[0].value.is_empty(), "stored before the flag existed");
        assert!(!rows[1].hidden && rows[1].value == "2");
        assert!(rows[2].hidden && rows[2].value.is_empty(), "a secret value is not in the page");
    }

    #[test]
    fn var_keys() {
        assert!(valid_var_key("DOCKER_TLS_CERTDIR"));
        assert!(valid_var_key("_x1"));
        assert!(!valid_var_key("1X"));
        assert!(!valid_var_key("A-B"));
    }
}
