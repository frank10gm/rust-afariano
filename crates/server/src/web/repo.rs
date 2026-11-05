use std::collections::HashMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::{PipelineRow, RepoNav, load_repo, markdown, render, user_names};
use crate::access::{self, Role};
use crate::app::Shared;
use crate::auth::{AdminUser, CurrentUser};
use crate::error::{AppError, AppResult};
use crate::git::{Commit, looks_binary};
use crate::models::{PIPELINE_COLS, Pipeline, REPO_COLS, Repo, User};
use crate::util::{ago, short_sha};

// ---------------------------------------------------------------- home

struct RepoRow {
    path: String,
    ns: String,
    name: String,
    hue: u16,
    initial: String,
    description: String,
    activity: String,
    status: String,
    status_label: &'static str,
    status_url: String,
    mirror: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    user: User,
    repos: Vec<RepoRow>,
    pipelines: Vec<PipelineRow>,
    show_repo: bool,
}

pub async fn home(State(state): State<Shared>, CurrentUser(user): CurrentUser) -> AppResult<Response> {
    // None: an administrator, who sees every repository.
    let visible = access::visible_repo_ids(&state, &user).await?;
    let repos: Vec<Repo> = sqlx::query_as(sql!(
        "SELECT {REPO_COLS} FROM repos WHERE $1::bigint[] IS NULL OR id = ANY($1) \
         ORDER BY COALESCE(pushed_at, created_at) DESC"
    ))
    .bind(&visible)
    .fetch_all(&state.db)
    .await?;
    let latest: Vec<(i64, i32, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (repo_id) repo_id, iid, status FROM pipelines \
         WHERE $1::bigint[] IS NULL OR repo_id = ANY($1) ORDER BY repo_id, id DESC",
    )
    .bind(&visible)
    .fetch_all(&state.db)
    .await?;
    let latest: HashMap<i64, (i32, String)> = latest.into_iter().map(|(r, i, s)| (r, (i, s))).collect();
    let paths: HashMap<i64, String> = repos.iter().map(|r| (r.id, r.path())).collect();
    let rows = repos
        .iter()
        .map(|r| {
            let (status, status_url) = latest
                .get(&r.id)
                .map(|(iid, s)| (s.clone(), format!("/{}/-/pipelines/{iid}", r.path())))
                .unwrap_or_default();
            RepoRow {
                path: r.path(),
                ns: r.namespace.clone(),
                name: r.name.clone(),
                hue: super::hue(&r.path()),
                initial: super::initial(&r.name),
                description: r.description.clone(),
                activity: ago(r.pushed_at.unwrap_or(r.created_at)),
                status_label: super::status_label(&status, true),
                status,
                status_url,
                mirror: r.mirror_url.as_deref().map(super::mirror_display).unwrap_or_default(),
            }
        })
        .collect();
    let recent: Vec<Pipeline> = sqlx::query_as(sql!(
        "SELECT {PIPELINE_COLS} FROM pipelines WHERE $1::bigint[] IS NULL OR repo_id = ANY($1) ORDER BY id DESC LIMIT 15"
    ))
    .bind(&visible)
    .fetch_all(&state.db)
    .await?;
    let names = user_names(&state, recent.iter().filter_map(|p| p.created_by)).await?;
    let mut pipelines: Vec<PipelineRow> = recent
        .iter()
        .filter_map(|p| {
            let path = paths.get(&p.repo_id)?;
            let repo = repos.iter().find(|r| r.id == p.repo_id)?;
            Some(
                PipelineRow::new(p, path, p.created_by.and_then(|id| names.get(&id)).map(String::as_str))
                    .for_repo(repo),
            )
        })
        .collect();
    super::attach_stages(&state, &mut pipelines).await?;
    render(HomePage { user, repos: rows, pipelines, show_repo: true })
}

// ---------------------------------------------------------------- new repo

#[derive(Template)]
#[template(path = "repo_new.html")]
struct NewRepoPage {
    user: User,
    error: String,
    namespace: String,
    name: String,
    description: String,
    default_branch: String,
    mirror_url: String,
}

pub async fn new_form(AdminUser(user): AdminUser) -> AppResult<Response> {
    let ns = user
        .email
        .split('@')
        .next()
        .unwrap_or("me")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    render(NewRepoPage {
        user,
        error: String::new(),
        namespace: ns,
        name: String::new(),
        description: String::new(),
        default_branch: "main".into(),
        mirror_url: String::new(),
    })
}

#[derive(Deserialize)]
pub struct NewRepoForm {
    namespace: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    mirror_url: String,
}

pub async fn create(
    State(state): State<Shared>,
    AdminUser(user): AdminUser,
    Form(f): Form<NewRepoForm>,
) -> AppResult<Response> {
    let ns = f.namespace.trim().to_string();
    let name = f.name.trim().to_string();
    let branch =
        if f.default_branch.trim().is_empty() { "main".to_string() } else { f.default_branch.trim().to_string() };
    let mirror_url = f.mirror_url.trim().to_string();
    let page = |user, error: &str, f: NewRepoForm| NewRepoPage {
        user,
        error: error.into(),
        namespace: f.namespace.trim().to_string(),
        name: f.name.trim().to_string(),
        description: f.description,
        default_branch: branch.clone(),
        mirror_url: f.mirror_url,
    };
    if !mirror_url.is_empty()
        && let Err(e) = crate::mirror::Upstream::parse(&mirror_url)
    {
        return render(page(user, e, f));
    }
    let id = match crate::ops::create_repo(&state, &user, &ns, &name, &f.description, &branch).await? {
        Ok(id) => id,
        Err(error) => return render(page(user, error, f)),
    };
    if mirror_url.is_empty() {
        return Ok(Redirect::to(&format!("/{ns}/{name}")).into_response());
    }
    let repo = state.repo_by_id(id).await?;
    let input = crate::mirror::MirrorInput { url: mirror_url, branches: None, https_token: None, status_token: None };
    if let Err(e) = crate::mirror::configure(&state, &repo, input).await? {
        return Err(AppError::BadRequest(e.into()));
    }
    crate::mirror::request_sync(&state, id);
    Ok(Redirect::to(&format!("/{ns}/{name}/-/settings#mirror")).into_response())
}

// ---------------------------------------------------------------- code

struct Crumb {
    name: String,
    url: String,
}

struct EntryRow {
    name: String,
    url: String,
    is_dir: bool,
    size: String,
}

struct CommitRow {
    sha: String,
    short: String,
    url: String,
    subject: String,
    body: String,
    author: String,
    author_initial: String,
    when: String,
    status: String,
    status_label: &'static str,
    status_url: String,
}

impl CommitRow {
    fn new(c: &Commit, repo_path: &str) -> Self {
        Self {
            sha: c.sha.clone(),
            short: short_sha(&c.sha).to_string(),
            url: format!("/{repo_path}/-/commit/{}", c.sha),
            subject: c.subject.clone(),
            body: c.body.clone(),
            author: c.author_name.clone(),
            author_initial: super::initial(&c.author_name),
            when: chrono::DateTime::from_timestamp(c.time, 0).map(ago).unwrap_or_default(),
            status: String::new(),
            status_label: "",
            status_url: String::new(),
        }
    }
}

#[derive(Template)]
#[template(path = "tree.html")]
struct TreePage {
    user: User,
    nav: RepoNav,
    empty: bool,
    ssh_url: String,
    http_url: String,
    git_ref: String,
    branches: Vec<String>,
    tags: Vec<String>,
    crumbs: Vec<Crumb>,
    entries: Vec<EntryRow>,
    last_commit: Option<CommitRow>,
    pipeline: Option<PipelineRow>,
    readme: String,
    readme_name: String,
    commit_count: u64,
    root: bool,
    history_url: String,
    /// For a mirror: when it last synced, and the error of the last attempt.
    mirror_synced: String,
    mirror_error: String,
}

fn human_size(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KiB", n as f64 / 1024.0),
        _ => format!("{:.1} MiB", n as f64 / 1_048_576.0),
    }
}

fn crumbs(repo_path: &str, git_ref: &str, path: &str) -> Vec<Crumb> {
    let mut out = vec![Crumb {
        name: repo_path.rsplit('/').next().unwrap_or(repo_path).to_string(),
        url: format!("/{repo_path}/-/tree/{git_ref}"),
    }];
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        out.push(Crumb { name: part.to_string(), url: format!("/{repo_path}/-/tree/{git_ref}/{acc}") });
    }
    out
}

pub async fn overview(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let git_ref = repo.default_branch.clone();
    tree_page(state, user, repo, role, git_ref, String::new()).await
}

pub async fn tree(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, rest)): Path<(String, String, String)>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let (git_ref, path) = state.git(repo.id).split_ref_path(&rest).await?.ok_or(AppError::NotFound)?;
    tree_page(state, user, repo, role, git_ref, path).await
}

async fn tree_page(
    state: Shared,
    user: User,
    repo: Repo,
    role: Role,
    git_ref: String,
    path: String,
) -> AppResult<Response> {
    let git = state.git(repo.id);
    let refs = git.refs().await?;
    let rp = repo.path();
    let mirror = match repo.is_mirror() {
        true => crate::mirror::load(&state, repo.id).await?,
        false => None,
    };
    let mut page = TreePage {
        mirror_synced: mirror.as_ref().and_then(|m| m.synced_at).map(ago).unwrap_or_else(|| "never".into()),
        mirror_error: mirror.and_then(|m| m.error).unwrap_or_default(),
        user,
        nav: RepoNav::new(&repo, role, "code"),
        empty: refs.is_empty(),
        ssh_url: state.cfg.ssh_clone_url(&rp),
        http_url: state.cfg.http_clone_url(&rp),
        git_ref: git_ref.clone(),
        branches: refs.iter().filter(|r| !r.is_tag()).map(|r| r.short().to_string()).collect(),
        tags: refs.iter().filter(|r| r.is_tag()).map(|r| r.short().to_string()).collect(),
        crumbs: crumbs(&rp, &git_ref, &path),
        entries: Vec::new(),
        last_commit: None,
        pipeline: None,
        readme: String::new(),
        readme_name: String::new(),
        commit_count: 0,
        root: path.is_empty(),
        history_url: if path.is_empty() {
            format!("/{rp}/-/commits/{git_ref}")
        } else {
            format!("/{rp}/-/commits/{git_ref}/{path}")
        },
    };
    if page.empty {
        return render(page);
    }
    let Some(sha) = git.resolve(&git_ref).await? else {
        if path.is_empty() {
            // Default branch missing but other refs exist: show the first branch.
            if let Some(first) = page.branches.first() {
                return Ok(Redirect::to(&format!("/{rp}/-/tree/{first}")).into_response());
            }
        }
        return Err(AppError::NotFound);
    };
    match git.object_kind(&sha, &path).await?.as_deref() {
        Some("tree") => {}
        Some("blob") => return Ok(Redirect::to(&format!("/{rp}/-/blob/{git_ref}/{path}")).into_response()),
        _ if path.is_empty() => {}
        _ => return Err(AppError::NotFound),
    }
    let entries = git.ls_tree(&sha, &path).await?;
    let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
    page.entries = entries
        .iter()
        .map(|e| {
            let is_dir = e.kind == "tree";
            let kind = if is_dir { "tree" } else { "blob" };
            EntryRow {
                name: e.name.clone(),
                url: format!("/{rp}/-/{kind}/{git_ref}/{prefix}{}", e.name),
                is_dir,
                size: e.size.map(human_size).unwrap_or_default(),
            }
        })
        .collect();
    page.last_commit = git.log(&sha, Some(&path), 0, 1).await?.first().map(|c| CommitRow::new(c, &rp));
    if path.is_empty() {
        page.commit_count = git.commit_count(&sha).await?;
        let p: Option<Pipeline> = sqlx::query_as(sql!(
            "SELECT {PIPELINE_COLS} FROM pipelines WHERE repo_id = $1 AND sha = $2 ORDER BY id DESC LIMIT 1"
        ))
        .bind(repo.id)
        .bind(&sha)
        .fetch_optional(&state.db)
        .await?;
        page.pipeline = p.map(|p| PipelineRow::new(&p, &rp, None));
    }
    if let Some(readme) = entries.iter().find(|e| e.kind == "blob" && e.name.to_lowercase().starts_with("readme"))
        && let Some((_, data)) = git.blob(&sha, &format!("{prefix}{}", readme.name), 512 << 10).await?
    {
        let text = String::from_utf8_lossy(&data);
        let dir = path.trim_matches('/');
        let sep = if dir.is_empty() { "" } else { "/" };
        page.readme = if readme.name.to_lowercase().ends_with(".md")
            || readme.name.to_lowercase().ends_with(".markdown")
        {
            markdown(&text, &format!("/{rp}/-/blob/{git_ref}{sep}{dir}"), &format!("/{rp}/-/raw/{git_ref}{sep}{dir}"))
        } else {
            format!("<pre>{}</pre>", crate::error::html_escape(&text))
        };
        page.readme_name = readme.name.clone();
    }
    render(page)
}

#[derive(Template)]
#[template(path = "blob.html")]
struct BlobPage {
    user: User,
    nav: RepoNav,
    git_ref: String,
    crumbs: Vec<Crumb>,
    file_name: String,
    size: String,
    lines: Vec<String>,
    binary: bool,
    too_large: bool,
    rendered: String,
    raw_url: String,
    history_url: String,
}

pub async fn blob(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, rest)): Path<(String, String, String)>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let git = state.git(repo.id);
    let (git_ref, path) = git.split_ref_path(&rest).await?.ok_or(AppError::NotFound)?;
    let rp = repo.path();
    if git.object_kind(&git_ref, &path).await?.as_deref() == Some("tree") {
        return Ok(Redirect::to(&format!("/{rp}/-/tree/{git_ref}/{path}")).into_response());
    }
    const VIEW_LIMIT: usize = 1 << 20;
    let (size, data) = git.blob(&git_ref, &path, VIEW_LIMIT).await?.ok_or(AppError::NotFound)?;
    let binary = looks_binary(&data);
    let too_large = size as usize > VIEW_LIMIT;
    let file_name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let text = if binary || too_large { String::new() } else { String::from_utf8_lossy(&data).into_owned() };
    let lower = file_name.to_lowercase();
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let sep = if dir.is_empty() { "" } else { "/" };
    let rendered = if !text.is_empty() && (lower.ends_with(".md") || lower.ends_with(".markdown")) {
        markdown(&text, &format!("/{rp}/-/blob/{git_ref}{sep}{dir}"), &format!("/{rp}/-/raw/{git_ref}{sep}{dir}"))
    } else {
        String::new()
    };
    let mut crumbs = crumbs(&rp, &git_ref, dir);
    crumbs.push(Crumb { name: file_name.clone(), url: String::new() });
    render(BlobPage {
        user,
        nav: RepoNav::new(&repo, role, "code"),
        git_ref: git_ref.clone(),
        crumbs,
        file_name,
        size: human_size(size),
        lines: text.lines().map(String::from).collect(),
        binary,
        too_large,
        rendered,
        raw_url: format!("/{rp}/-/raw/{git_ref}/{path}"),
        history_url: format!("/{rp}/-/commits/{git_ref}/{path}"),
    })
}

/// Raw file. Always `text/plain` or `application/octet-stream` (plus nosniff
/// and a sandboxing CSP) so repository content never runs as a page.
pub async fn raw(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, rest)): Path<(String, String, String)>,
) -> AppResult<Response> {
    let (repo, _role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let git = state.git(repo.id);
    let (git_ref, path) = git.split_ref_path(&rest).await?.ok_or(AppError::NotFound)?;
    let (_, data) = git.blob(&git_ref, &path, 64 << 20).await?.ok_or(AppError::NotFound)?;
    let lower = path.to_lowercase();
    let ctype = if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if looks_binary(&data) {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    };
    Ok((
        [
            (header::CONTENT_TYPE, ctype),
            (header::CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'; img-src 'self'"),
        ],
        data,
    )
        .into_response())
}

// ---------------------------------------------------------------- commits

#[derive(Template)]
#[template(path = "commits.html")]
struct CommitsPage {
    user: User,
    nav: RepoNav,
    git_ref: String,
    path: String,
    commits: Vec<CommitRow>,
    prev_url: String,
    next_url: String,
}

#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    page: usize,
}

pub async fn commits_default(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let r = repo.default_branch.clone();
    commits_page(state, user, repo, role, r, String::new(), q.page).await
}

pub async fn commits(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, rest)): Path<(String, String, String)>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let (git_ref, path) = state.git(repo.id).split_ref_path(&rest).await?.ok_or(AppError::NotFound)?;
    commits_page(state, user, repo, role, git_ref, path, q.page).await
}

async fn commits_page(
    state: Shared,
    user: User,
    repo: Repo,
    role: Role,
    git_ref: String,
    path: String,
    page: usize,
) -> AppResult<Response> {
    const PER_PAGE: usize = 40;
    let git = state.git(repo.id);
    let rp = repo.path();
    let list = if git.resolve(&git_ref).await?.is_some() {
        git.log(&git_ref, Some(&path), page * PER_PAGE, PER_PAGE + 1).await?
    } else {
        Vec::new()
    };
    let has_next = list.len() > PER_PAGE;
    let mut commits: Vec<CommitRow> = list.iter().take(PER_PAGE).map(|c| CommitRow::new(c, &rp)).collect();
    let shas: Vec<String> = commits.iter().map(|c| c.sha.clone()).collect();
    let rows: Vec<(String, i32, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (sha) sha, iid, status FROM pipelines WHERE repo_id = $1 AND sha = ANY($2) ORDER BY sha, id DESC",
    )
    .bind(repo.id)
    .bind(&shas)
    .fetch_all(&state.db)
    .await?;
    let statuses: HashMap<String, (String, i32)> = rows.into_iter().map(|(sha, iid, st)| (sha, (st, iid))).collect();
    for c in &mut commits {
        if let Some((st, iid)) = statuses.get(&c.sha) {
            c.status = st.clone();
            c.status_label = super::status_label(st, true);
            c.status_url = format!("/{rp}/-/pipelines/{iid}");
        }
    }
    let base = if path.is_empty() {
        format!("/{rp}/-/commits/{git_ref}")
    } else {
        format!("/{rp}/-/commits/{git_ref}/{path}")
    };
    render(CommitsPage {
        user,
        nav: RepoNav::new(&repo, role, "commits"),
        git_ref,
        path,
        commits,
        prev_url: if page > 0 { format!("{base}?page={}", page - 1) } else { String::new() },
        next_url: if has_next { format!("{base}?page={}", page + 1) } else { String::new() },
    })
}

struct DiffLine {
    class: &'static str,
    old: String,
    new: String,
    text: String,
}

struct DiffFile {
    name: String,
    adds: usize,
    dels: usize,
    lines: Vec<DiffLine>,
    binary: bool,
}

#[derive(Template)]
#[template(path = "commit.html")]
struct CommitPage {
    user: User,
    nav: RepoNav,
    commit: CommitRow,
    parents: Vec<(String, String)>,
    files: Vec<DiffFile>,
    truncated: bool,
    tree_url: String,
    pipeline: Option<PipelineRow>,
}

fn parse_diff(diff: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let (mut old_no, mut new_no) = (0usize, 0usize);
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let name = rest.split(" b/").last().unwrap_or(rest).to_string();
            files.push(DiffFile { name, adds: 0, dels: 0, lines: Vec::new(), binary: false });
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if line.starts_with("Binary files") {
            f.binary = true;
        } else if line.starts_with("@@") {
            // @@ -a,b +c,d @@
            let nums: Vec<usize> = line
                .split_whitespace()
                .skip(1)
                .take(2)
                .map(|s| s.trim_start_matches(['-', '+']).split(',').next().unwrap_or("0").parse().unwrap_or(0))
                .collect();
            old_no = nums.first().copied().unwrap_or(0);
            new_no = nums.get(1).copied().unwrap_or(0);
            f.lines.push(DiffLine { class: "hunk", old: String::new(), new: String::new(), text: line.to_string() });
        } else if f.lines.is_empty() {
            // Header lines (index, ---, +++, mode, rename) before the first hunk.
            if let Some(to) = line.strip_prefix("rename to ") {
                f.name = to.to_string();
            }
        } else if let Some(t) = line.strip_prefix('+') {
            f.adds += 1;
            f.lines.push(DiffLine { class: "add", old: String::new(), new: new_no.to_string(), text: format!("+{t}") });
            new_no += 1;
        } else if let Some(t) = line.strip_prefix('-') {
            f.dels += 1;
            f.lines.push(DiffLine { class: "del", old: old_no.to_string(), new: String::new(), text: format!("-{t}") });
            old_no += 1;
        } else if line.starts_with('\\') {
            f.lines.push(DiffLine { class: "hunk", old: String::new(), new: String::new(), text: line.to_string() });
        } else {
            f.lines.push(DiffLine {
                class: "",
                old: old_no.to_string(),
                new: new_no.to_string(),
                text: line.to_string(),
            });
            old_no += 1;
            new_no += 1;
        }
    }
    files
}

pub async fn commit(
    State(state): State<Shared>,
    CurrentUser(user): CurrentUser,
    Path((ns, name, sha)): Path<(String, String, String)>,
) -> AppResult<Response> {
    let (repo, role) = load_repo(&state, &user, &ns, &name, Role::Read).await?;
    let git = state.git(repo.id);
    let full = git.resolve(&sha).await?.ok_or(AppError::NotFound)?;
    let c = git.commit(&full).await?.ok_or(AppError::NotFound)?;
    const DIFF_LIMIT: usize = 2 << 20;
    let diff = git.diff(&full, DIFF_LIMIT).await?;
    let truncated = diff.len() >= DIFF_LIMIT;
    let rp = repo.path();
    let p: Option<Pipeline> = sqlx::query_as(sql!(
        "SELECT {PIPELINE_COLS} FROM pipelines WHERE repo_id = $1 AND sha = $2 ORDER BY id DESC LIMIT 1"
    ))
    .bind(repo.id)
    .bind(&full)
    .fetch_optional(&state.db)
    .await?;
    render(CommitPage {
        user,
        nav: RepoNav::new(&repo, role, "commits"),
        parents: c.parents.iter().map(|p| (short_sha(p).to_string(), format!("/{rp}/-/commit/{p}"))).collect(),
        commit: CommitRow::new(&c, &rp),
        files: parse_diff(&String::from_utf8_lossy(&diff)),
        truncated,
        tree_url: format!("/{rp}/-/tree/{full}"),
        pipeline: p.map(|p| PipelineRow::new(&p, &rp, None)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::valid_branch;

    #[test]
    fn branches() {
        assert!(valid_branch("main"));
        assert!(valid_branch("feature/x-1"));
        assert!(!valid_branch("-x"));
        assert!(!valid_branch("a..b"));
        assert!(!valid_branch("a b"));
        assert!(!valid_branch("x.lock"));
    }

    #[test]
    fn diff_parsing() {
        let d =
            "diff --git a/x.txt b/x.txt\nindex 1..2 100644\n--- a/x.txt\n+++ b/x.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n";
        let f = parse_diff(d);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "x.txt");
        assert_eq!((f[0].adds, f[0].dels), (1, 1));
        assert_eq!(f[0].lines.len(), 4);
        assert_eq!(f[0].lines[3].new, "2");
    }
}
