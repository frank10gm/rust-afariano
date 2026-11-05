//! Pipeline lifecycle: create from a ref, advance job states, hand jobs to
//! runners, finish/cancel/retry. Semantics in docs/DESIGN.md § Scheduling.

use std::collections::{BTreeMap, HashMap};

use anyhow::Context as _;
use rust_afariano_ci as ci;
use rust_afariano_proto as proto;
use sqlx::types::Json;
use sqlx::{Postgres, Transaction};

use crate::app::AppState;
use crate::git::Commit;
use crate::models::{JOB_COLS, Job, PIPELINE_COLS, Pipeline, PipelineVar, Repo, Runner, User, Variable};
use crate::util::{glob_match, hash_token, new_token, ref_slug, short_sha};

pub const CI_FILE: &str = ".gitlab-ci.yml";
const DEFAULT_TIMEOUT_SECS: u64 = 3600;
const MAX_TIMEOUT_SECS: u64 = 24 * 3600;

pub struct NewPipeline<'a> {
    pub repo: &'a Repo,
    pub ref_name: String,
    pub is_tag: bool,
    pub sha: String,
    pub before_sha: String,
    /// `push`, `web` or `api`.
    pub source: &'static str,
    pub user: Option<&'a User>,
    pub variables: Vec<PipelineVar>,
}

#[derive(Debug)]
pub enum Created {
    /// Pipeline `iid` (it may already be `failed` with a YAML error).
    Pipeline(i32),
    /// Workflow rules / job rules produced nothing.
    Filtered(String),
    /// No `.gitlab-ci.yml` at that commit.
    NoConfig,
}

/// Facts about the ref/commit/user every predefined variable is built from.
struct RefFacts<'a> {
    repo: &'a Repo,
    ref_name: &'a str,
    is_tag: bool,
    sha: &'a str,
    before_sha: &'a str,
    source: &'a str,
    commit: Option<&'a Commit>,
    user: Option<&'a User>,
}

fn predefined(state: &AppState, f: &RefFacts<'_>) -> Vec<(String, String)> {
    let cfg = &state.cfg;
    let path = f.repo.path();
    let mut v: Vec<(String, String)> = vec![
        ("CI".into(), "true".into()),
        ("GITLAB_CI".into(), "true".into()),
        ("CI_SERVER".into(), "yes".into()),
        ("CI_SERVER_NAME".into(), "rust-afariano".into()),
        ("CI_SERVER_VERSION".into(), env!("CARGO_PKG_VERSION").into()),
        ("CI_SERVER_URL".into(), cfg.public_url.clone()),
        ("CI_SERVER_HOST".into(), cfg.public_host().into()),
        ("CI_SERVER_PROTOCOL".into(), cfg.public_url.split("://").next().unwrap_or("https").into()),
        ("CI_CONFIG_PATH".into(), CI_FILE.into()),
        ("CI_PIPELINE_SOURCE".into(), f.source.into()),
        ("CI_PROJECT_ID".into(), f.repo.id.to_string()),
        ("CI_PROJECT_NAME".into(), f.repo.name.clone()),
        ("CI_PROJECT_TITLE".into(), f.repo.name.clone()),
        ("CI_PROJECT_NAMESPACE".into(), f.repo.namespace.clone()),
        ("CI_PROJECT_ROOT_NAMESPACE".into(), f.repo.namespace.clone()),
        ("CI_PROJECT_PATH".into(), path.clone()),
        ("CI_PROJECT_PATH_SLUG".into(), ref_slug(&path)),
        ("CI_PROJECT_URL".into(), format!("{}/{path}", cfg.public_url)),
        ("CI_PROJECT_VISIBILITY".into(), "private".into()),
        ("CI_DEFAULT_BRANCH".into(), f.repo.default_branch.clone()),
        ("CI_COMMIT_SHA".into(), f.sha.into()),
        ("CI_COMMIT_SHORT_SHA".into(), short_sha(f.sha).into()),
        ("CI_COMMIT_BEFORE_SHA".into(), f.before_sha.into()),
        ("CI_COMMIT_REF_NAME".into(), f.ref_name.into()),
        ("CI_COMMIT_REF_SLUG".into(), ref_slug(f.ref_name)),
        ("CI_COMMIT_REF_PROTECTED".into(), f.repo.is_protected(f.ref_name, f.is_tag).to_string()),
    ];
    if f.is_tag {
        v.push(("CI_COMMIT_TAG".into(), f.ref_name.into()));
    } else {
        v.push(("CI_COMMIT_BRANCH".into(), f.ref_name.into()));
    }
    if let Some(c) = f.commit {
        let message = if c.body.is_empty() { c.subject.clone() } else { format!("{}\n\n{}", c.subject, c.body) };
        v.push(("CI_COMMIT_TITLE".into(), c.subject.clone()));
        v.push(("CI_COMMIT_MESSAGE".into(), message));
        v.push(("CI_COMMIT_DESCRIPTION".into(), c.body.clone()));
        v.push(("CI_COMMIT_AUTHOR".into(), format!("{} <{}>", c.author_name, c.author_email)));
        if let Some(t) = chrono::DateTime::from_timestamp(c.time, 0) {
            v.push(("CI_COMMIT_TIMESTAMP".into(), t.to_rfc3339()));
        }
    }
    if let Some(u) = f.user {
        v.push(("GITLAB_USER_ID".into(), u.id.to_string()));
        v.push(("GITLAB_USER_EMAIL".into(), u.email.clone()));
        v.push(("GITLAB_USER_LOGIN".into(), u.email.split('@').next().unwrap_or(&u.email).into()));
        v.push(("GITLAB_USER_NAME".into(), u.display().into()));
    }
    v
}

async fn project_variables(state: &AppState, repo_id: i64) -> anyhow::Result<Vec<Variable>> {
    let mut vars = sqlx::query_as::<_, Variable>(
        "SELECT id, repo_id, key, value, file, protected, masked, environment_scope FROM variables \
         WHERE repo_id = $1 ORDER BY (environment_scope = '*') DESC, key",
    )
    .bind(repo_id)
    .fetch_all(&state.db)
    .await?;
    for v in &mut vars {
        v.value = state.cipher.decrypt(&v.value)?;
    }
    Ok(vars)
}

/// Project variables visible on this ref (and environment, if any). `*` scope
/// first so a specific scope overrides it.
fn visible_variables<'a>(vars: &'a [Variable], protected_ref: bool, environment: Option<&str>) -> Vec<&'a Variable> {
    vars.iter()
        .filter(|v| protected_ref || !v.protected)
        .filter(|v| v.environment_scope == "*" || environment.is_some_and(|e| glob_match(&v.environment_scope, e)))
        .collect()
}

pub async fn create(state: &AppState, np: NewPipeline<'_>) -> anyhow::Result<Created> {
    let git = state.git(np.repo.id);
    let Some((_, yaml)) = git.blob(&np.sha, CI_FILE, 1 << 20).await? else {
        return Ok(Created::NoConfig);
    };
    let yaml = String::from_utf8_lossy(&yaml).into_owned();
    let commit = git.commit(&np.sha).await?;
    let facts = RefFacts {
        repo: np.repo,
        ref_name: &np.ref_name,
        is_tag: np.is_tag,
        sha: &np.sha,
        before_sha: &np.before_sha,
        source: np.source,
        commit: commit.as_ref(),
        user: np.user,
    };
    let mut ctx = ci::Context::default();
    ctx.variables.extend(predefined(state, &facts));
    let project_vars = project_variables(state, np.repo.id).await?;
    let protected = np.repo.is_protected(&np.ref_name, np.is_tag);
    for v in visible_variables(&project_vars, protected, None) {
        ctx.variables.insert(v.key.clone(), v.value.clone());
    }
    for v in &np.variables {
        ctx.variables.insert(v.key.clone(), v.value.clone());
    }

    let (plan, error) = match ci::plan(&yaml, &ctx) {
        Ok(ci::Outcome::NoPipeline { reason }) => return Ok(Created::Filtered(reason)),
        Ok(ci::Outcome::Empty) => return Ok(Created::Filtered("no job matched its rules".into())),
        Ok(ci::Outcome::Pipeline(plan)) => (Some(plan), None),
        Err(e) => (None, Some(e.to_string())),
    };

    let mut tx = state.db.begin().await?;
    let iid: i32 =
        sqlx::query_scalar("UPDATE repos SET pipeline_seq = pipeline_seq + 1 WHERE id = $1 RETURNING pipeline_seq")
            .bind(np.repo.id)
            .fetch_one(&mut *tx)
            .await?;
    let status = if error.is_some() { "failed" } else { "created" };
    let (stages, yaml_vars) = plan.as_ref().map(|p| (p.stages.clone(), p.variables.clone())).unwrap_or_default();
    let pipeline_id: i64 = sqlx::query_scalar(
        "INSERT INTO pipelines (repo_id, iid, ref_name, ref_is_tag, sha, before_sha, source, status, variables, stages, \
         yaml_vars, error, commit_title, created_by, finished_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14, CASE WHEN $8 = 'failed' THEN now() END) RETURNING id",
    )
    .bind(np.repo.id)
    .bind(iid)
    .bind(&np.ref_name)
    .bind(np.is_tag)
    .bind(&np.sha)
    .bind(&np.before_sha)
    .bind(np.source)
    .bind(status)
    .bind(Json(&np.variables))
    .bind(&stages)
    .bind(Json(&yaml_vars))
    .bind(&error)
    .bind(commit.as_ref().map(|c| c.subject.as_str()).unwrap_or(""))
    .bind(np.user.map(|u| u.id))
    .fetch_one(&mut *tx)
    .await?;
    if let Some(plan) = &plan {
        for job in &plan.jobs {
            sqlx::query(
                "INSERT INTO jobs (pipeline_id, repo_id, name, stage, stage_idx, status, \"when\", allow_failure, needs, \
                 tags, resource_group, environment, spec) VALUES ($1,$2,$3,$4,$5,'created',$6,$7,$8,$9,$10,$11,$12)",
            )
            .bind(pipeline_id)
            .bind(np.repo.id)
            .bind(&job.name)
            .bind(&job.stage)
            .bind(job.stage_idx as i32)
            .bind(when_str(job.when))
            .bind(job.allow_failure)
            .bind(&job.needs)
            .bind(&job.tags)
            .bind(&job.resource_group)
            .bind(job.environment.as_ref().map(|e| e.name.as_str()))
            .bind(Json(job))
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    git.keep_around(pipeline_id, &np.sha).await;
    if plan.is_some() {
        advance(state, pipeline_id).await?;
    }
    crate::commit_status::notify(state, pipeline_id);
    tracing::info!(repo = %np.repo.path(), iid, r#ref = %np.ref_name, source = np.source, "pipeline created");
    Ok(Created::Pipeline(iid))
}

fn when_str(w: ci::When) -> &'static str {
    match w {
        ci::When::OnSuccess => "on_success",
        ci::When::OnFailure => "on_failure",
        ci::When::Always => "always",
        ci::When::Manual => "manual",
    }
}

async fn load_jobs(tx: &mut Transaction<'_, Postgres>, pipeline_id: i64) -> sqlx::Result<Vec<Job>> {
    sqlx::query_as::<_, Job>(sql!(
        "SELECT {JOB_COLS} FROM jobs WHERE pipeline_id = $1 AND NOT retried ORDER BY stage_idx, id"
    ))
    .bind(pipeline_id)
    .fetch_all(&mut **tx)
    .await
}

/// Next status of a `created` job given its dependencies, or `None` to keep waiting.
fn decide(job: &Job, jobs: &[Job], status: &HashMap<i64, String>) -> Option<&'static str> {
    let by_name: HashMap<&str, &Job> = jobs.iter().map(|j| (j.name.as_str(), j)).collect();
    let deps: Vec<(&Job, bool)> = match &job.needs {
        Some(needs) => needs.iter().filter_map(|n| by_name.get(n.as_str()).map(|j| (*j, true))).collect(),
        None => jobs.iter().filter(|j| j.stage_idx < job.stage_idx).map(|j| (j, false)).collect(),
    };
    let mut ok = true;
    for (dep, is_need) in deps {
        let s = status[&dep.id].as_str();
        match s {
            "success" => {}
            "skipped" if is_need => ok = false,
            "skipped" => {}
            "failed" if dep.allow_failure => {}
            "failed" | "canceled" => ok = false,
            "manual" if dep.allow_failure => {}
            _ => return None, // created, pending, running, blocking manual
        }
    }
    Some(match (job.when.as_str(), ok) {
        ("on_success", true) | ("on_failure", false) | ("always", _) => "pending",
        ("manual", true) => "manual",
        _ => "skipped",
    })
}

fn pipeline_status(jobs: &[Job], status: &HashMap<i64, String>) -> &'static str {
    let st = |j: &Job| status[&j.id].as_str();
    if jobs.is_empty() {
        return "skipped";
    }
    if jobs.iter().any(|j| st(j) == "running") {
        return "running";
    }
    if jobs.iter().any(|j| st(j) == "pending") {
        let started = jobs.iter().any(|j| matches!(st(j), "success" | "failed" | "canceled"));
        return if started { "running" } else { "pending" };
    }
    if jobs.iter().any(|j| st(j) == "created" || (st(j) == "manual" && !j.allow_failure)) {
        return "manual";
    }
    if jobs.iter().any(|j| st(j) == "failed" && !j.allow_failure) {
        return "failed";
    }
    if jobs.iter().any(|j| st(j) == "canceled") {
        return "canceled";
    }
    if jobs.iter().all(|j| st(j) == "skipped") {
        return "skipped";
    }
    "success"
}

/// Move `created` jobs whose dependencies settled, then recompute the pipeline status.
pub async fn advance(state: &AppState, pipeline_id: i64) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let was: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1 FOR UPDATE")
        .bind(pipeline_id)
        .fetch_one(&mut *tx)
        .await?;
    let jobs = load_jobs(&mut tx, pipeline_id).await?;
    let mut status: HashMap<i64, String> = jobs.iter().map(|j| (j.id, j.status.clone())).collect();
    let mut changed: Vec<(i64, &'static str)> = Vec::new();
    loop {
        let mut progress = false;
        for job in &jobs {
            if status[&job.id] != "created" {
                continue;
            }
            if let Some(next) = decide(job, &jobs, &status) {
                status.insert(job.id, next.to_string());
                changed.push((job.id, next));
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    for (id, s) in &changed {
        sqlx::query(
            "UPDATE jobs SET status = $2, updated_at = now(), \
             queued_at = CASE WHEN $2 = 'pending' THEN now() ELSE queued_at END, \
             finished_at = CASE WHEN $2 = 'skipped' THEN now() ELSE finished_at END WHERE id = $1",
        )
        .bind(id)
        .bind(s)
        .execute(&mut *tx)
        .await?;
    }
    let p = pipeline_status(&jobs, &status);
    let terminal = matches!(p, "success" | "failed" | "canceled" | "skipped");
    sqlx::query(
        "UPDATE pipelines SET status = $2, \
         started_at = CASE WHEN $2 = 'running' THEN COALESCE(started_at, now()) ELSE started_at END, \
         finished_at = CASE WHEN $3 THEN COALESCE(finished_at, now()) ELSE NULL END WHERE id = $1",
    )
    .bind(pipeline_id)
    .bind(p)
    .bind(terminal)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if changed.iter().any(|(_, s)| *s == "pending") {
        state.jobs_ready.notify_waiters();
    }
    if was != p {
        crate::commit_status::notify(state, pipeline_id);
    }
    Ok(())
}

pub async fn get_job(state: &AppState, job_id: i64) -> sqlx::Result<Job> {
    sqlx::query_as::<_, Job>(sql!("SELECT {JOB_COLS} FROM jobs WHERE id = $1")).bind(job_id).fetch_one(&state.db).await
}

pub async fn get_pipeline(state: &AppState, pipeline_id: i64) -> sqlx::Result<Pipeline> {
    sqlx::query_as::<_, Pipeline>(sql!("SELECT {PIPELINE_COLS} FROM pipelines WHERE id = $1"))
        .bind(pipeline_id)
        .fetch_one(&state.db)
        .await
}

/// Take the oldest pending job this runner can run. One global advisory lock
/// keeps `resource_group` exclusive across concurrent requests.
pub async fn assign(state: &AppState, runner: &Runner) -> anyhow::Result<Option<proto::JobSpec>> {
    let token = new_token(proto::JOB_TOKEN_PREFIX);
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7378656970)").execute(&mut *tx).await?;
    let mut outdated: Vec<(i64, i64, String)> = Vec::new();
    let job_id = loop {
        let job_id: Option<i64> = sqlx::query_scalar(
            "SELECT j.id FROM jobs j WHERE j.status = 'pending' AND j.tags <@ $1::text[] \
             AND (cardinality(j.tags) > 0 OR $2) \
             AND (j.resource_group IS NULL OR NOT EXISTS (SELECT 1 FROM jobs r WHERE r.repo_id = j.repo_id \
                  AND r.resource_group = j.resource_group AND r.status = 'running')) \
             ORDER BY j.id LIMIT 1",
        )
        .bind(&runner.tags)
        .bind(runner.run_untagged)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(job_id) = job_id else { break None };
        match newer_deployment(&mut tx, job_id).await? {
            None => break Some(job_id),
            Some(note) => {
                let pipeline_id: i64 = sqlx::query_scalar(
                    "UPDATE jobs SET status = 'failed', failure_reason = 'outdated_deployment', finished_at = now(), \
                     updated_at = now() WHERE id = $1 RETURNING pipeline_id",
                )
                .bind(job_id)
                .fetch_one(&mut *tx)
                .await?;
                outdated.push((job_id, pipeline_id, note));
            }
        }
    };
    if !outdated.is_empty() {
        // Committed before telling anyone, so the failures are what they read.
        tx.commit().await?;
        for (id, pipeline_id, note) in outdated {
            state.logs.note(id, &note).await;
            state.logs.end(id);
            tracing::info!(job = id, "outdated deployment job not run");
            advance(state, pipeline_id).await?;
        }
        // Start over: the job picked last is still free to take.
        return Box::pin(assign(state, runner)).await;
    }
    let Some(job_id) = job_id else {
        tx.commit().await?;
        return Ok(None);
    };
    sqlx::query(
        "UPDATE jobs SET status = 'running', runner_id = $2, token_hash = $3, started_at = now(), updated_at = now(), \
         log_size = 0 WHERE id = $1",
    )
    .bind(job_id)
    .bind(runner.id)
    .bind(hash_token(&token))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let job = get_job(state, job_id).await?;
    advance(state, job.pipeline_id).await?;
    match job_spec(state, &job, runner, &token).await {
        Ok(spec) => {
            tracing::info!(job = job_id, runner = %runner.name, "job assigned");
            Ok(Some(spec))
        }
        Err(e) => {
            tracing::error!(job = job_id, "building job spec: {e:#}");
            state.logs.note(job_id, &format!("rust-afariano could not prepare this job: {e:#}")).await;
            finish(state, job_id, "failed", None, Some("system_failure")).await?;
            Ok(None)
        }
    }
}

/// GitLab's "Prevent outdated deployment jobs": a job deploying to an environment
/// does not start when a newer pipeline already deployed there successfully. A
/// retried job still runs: retrying an old deployment is how one rolls back.
/// Environments named with variables (review apps) are not compared.
/// Returns the note for the job's log when it must not run.
async fn newer_deployment(tx: &mut Transaction<'_, Postgres>, job_id: i64) -> sqlx::Result<Option<String>> {
    let newer: Option<(String, i32, String)> = sqlx::query_as(
        "SELECT j.environment, p.iid, p.sha FROM jobs j JOIN repos r ON r.id = j.repo_id \
         JOIN jobs d ON d.repo_id = j.repo_id AND d.environment = j.environment AND d.status = 'success' \
              AND d.pipeline_id > j.pipeline_id \
         JOIN pipelines p ON p.id = d.pipeline_id \
         WHERE j.id = $1 AND r.prevent_outdated_deploys AND j.retry_of IS NULL \
           AND j.environment IS NOT NULL AND strpos(j.environment, '$') = 0 \
         ORDER BY d.pipeline_id DESC LIMIT 1",
    )
    .bind(job_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(newer.map(|(env, iid, sha)| {
        format!(
            "Not run: pipeline #{iid} ({}) already deployed a newer version to {env}, and this job would replace it \
             with an older one (Settings → Prevent outdated deployment jobs). To deploy the latest version, run a \
             new pipeline. To go back to this version on purpose, retry this job.",
            short_sha(&sha)
        )
    }))
}

/// `$VAR` / `${VAR}` expansion with GitLab's rules: unknown → empty, `$$` → `$`.
pub fn expand(value: &str, vars: &BTreeMap<String, proto::Variable>) -> String {
    let mut out = String::with_capacity(value.len());
    let b = value.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            let ch = value[i..].chars().next().expect("char");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if b.get(i + 1) == Some(&b'$') {
            out.push('$');
            i += 2;
            continue;
        }
        let (name, next) = if b.get(i + 1) == Some(&b'{') {
            match value[i + 2..].find('}') {
                Some(end) => (&value[i + 2..i + 2 + end], i + 3 + end),
                None => {
                    out.push('$');
                    i += 1;
                    continue;
                }
            }
        } else {
            let len = value[i + 1..].bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
            (&value[i + 1..i + 1 + len], i + 1 + len)
        };
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            out.push('$');
            i += 1;
            continue;
        }
        if let Some(v) = vars.get(name) {
            out.push_str(&v.value);
        }
        i = next;
    }
    out
}

fn put(map: &mut BTreeMap<String, proto::Variable>, key: &str, value: impl Into<String>) {
    map.insert(
        key.to_string(),
        proto::Variable { key: key.to_string(), value: value.into(), file: false, masked: false },
    );
}

async fn job_spec(state: &AppState, job: &Job, runner: &Runner, token: &str) -> anyhow::Result<proto::JobSpec> {
    let pipeline = get_pipeline(state, job.pipeline_id).await?;
    let repo = state.repo_by_id(job.repo_id).await?;
    let user = match pipeline.created_by {
        Some(id) => {
            sqlx::query_as::<_, User>("SELECT id, email, name FROM users WHERE id = $1")
                .bind(id)
                .fetch_optional(&state.db)
                .await?
        }
        None => None,
    };
    let commit = state.git(repo.id).commit(&pipeline.sha).await?;
    let facts = RefFacts {
        repo: &repo,
        ref_name: &pipeline.ref_name,
        is_tag: pipeline.ref_is_tag,
        sha: &pipeline.sha,
        before_sha: &pipeline.before_sha,
        source: &pipeline.source,
        commit: commit.as_ref(),
        user: user.as_ref(),
    };
    let spec = &job.spec.0;
    let cfg = &state.cfg;
    let path = repo.path();
    let job_url = format!("{}/{path}/-/jobs/{}", cfg.public_url, job.id);

    let mut vars: BTreeMap<String, proto::Variable> = BTreeMap::new();
    for (k, v) in predefined(state, &facts) {
        put(&mut vars, &k, v);
    }
    put(&mut vars, "CI_PIPELINE_ID", pipeline.id.to_string());
    put(&mut vars, "CI_PIPELINE_IID", pipeline.iid.to_string());
    put(&mut vars, "CI_PIPELINE_URL", format!("{}/{path}/-/pipelines/{}", cfg.public_url, pipeline.iid));
    put(&mut vars, "CI_PIPELINE_CREATED_AT", pipeline.created_at.to_rfc3339());
    put(&mut vars, "CI_JOB_ID", job.id.to_string());
    put(&mut vars, "CI_JOB_NAME", &job.name);
    put(&mut vars, "CI_JOB_NAME_SLUG", ref_slug(&job.name));
    put(&mut vars, "CI_JOB_STAGE", &job.stage);
    put(&mut vars, "CI_JOB_URL", &job_url);
    put(&mut vars, "CI_JOB_STARTED_AT", chrono::Utc::now().to_rfc3339());
    put(&mut vars, "CI_NODE_TOTAL", "1");
    put(&mut vars, "CI_RUNNER_ID", runner.id.to_string());
    put(&mut vars, "CI_RUNNER_DESCRIPTION", &runner.name);
    put(&mut vars, "CI_RUNNER_TAGS", serde_json::to_string(&runner.tags)?);
    put(&mut vars, "CI_REGISTRY", ""); // no registry: scripts that log into it fail loudly
    vars.insert(
        "CI_JOB_TOKEN".into(),
        proto::Variable { key: "CI_JOB_TOKEN".into(), value: token.into(), file: false, masked: true },
    );
    let repo_url = cfg.http_clone_url(&path);
    let with_token = repo_url.replacen("://", &format!("://{}:{token}@", proto::CI_GIT_USER), 1);
    put(&mut vars, "CI_REPOSITORY_URL", with_token);
    if let Some(env) = &spec.environment {
        put(&mut vars, "CI_ENVIRONMENT_NAME", &env.name);
        put(&mut vars, "CI_ENVIRONMENT_SLUG", ref_slug(&env.name));
        if let Some(url) = &env.url {
            put(&mut vars, "CI_ENVIRONMENT_URL", url);
        }
    }

    // YAML variables expand references to what is already defined.
    for (k, v) in pipeline.yaml_vars.0.iter().chain(spec.variables.iter()) {
        let value = expand(v, &vars);
        put(&mut vars, k, value);
    }
    let project_vars = project_variables(state, repo.id).await?;
    let protected = repo.is_protected(&pipeline.ref_name, pipeline.ref_is_tag);
    let env_name = spec.environment.as_ref().map(|e| e.name.as_str());
    for v in visible_variables(&project_vars, protected, env_name) {
        vars.insert(
            v.key.clone(),
            proto::Variable { key: v.key.clone(), value: v.value.clone(), file: v.file, masked: v.masked },
        );
    }
    for v in &pipeline.variables.0 {
        // A secret run variable is masked in the job log like a masked project variable.
        vars.insert(
            v.key.clone(),
            proto::Variable { key: v.key.clone(), value: v.value.clone(), file: false, masked: v.is_secret() },
        );
    }

    let get = |k: &str| vars.get(k).map(|v| v.value.clone());
    let depth = get("GIT_DEPTH").and_then(|d| d.trim().parse().ok()).unwrap_or(20);
    let strategy = get("GIT_STRATEGY")
        .filter(|s| matches!(s.as_str(), "fetch" | "clone" | "none"))
        .unwrap_or_else(|| "fetch".into());
    let image = spec.image.clone().map(|mut i| {
        i.name = expand(&i.name, &vars);
        i
    });
    let services = spec
        .services
        .iter()
        .cloned()
        .map(|mut s| {
            s.name = expand(&s.name, &vars);
            s
        })
        .collect();
    let timeout_secs = spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(60, MAX_TIMEOUT_SECS);

    Ok(proto::JobSpec {
        id: job.id,
        token: token.to_string(),
        name: job.name.clone(),
        stage: job.stage.clone(),
        pipeline_id: pipeline.id,
        project_path: path,
        git: proto::GitSpec {
            url: repo_url,
            sha: pipeline.sha.clone(),
            ref_name: pipeline.ref_name.clone(),
            ref_is_tag: pipeline.ref_is_tag,
            depth,
            strategy,
        },
        image,
        services,
        before_script: spec.before_script.clone(),
        script: spec.script.clone(),
        after_script: spec.after_script.clone(),
        variables: vars.into_values().collect(),
        timeout_secs,
    })
}

/// Terminal transition of a running/pending job. `status`: success/failed/canceled.
pub async fn finish(
    state: &AppState,
    job_id: i64,
    status: &str,
    exit_code: Option<i32>,
    failure_reason: Option<&str>,
) -> anyhow::Result<()> {
    let pipeline_id: Option<i64> = sqlx::query_scalar(
        "UPDATE jobs SET status = CASE WHEN cancel_requested THEN 'canceled' ELSE $2 END, exit_code = $3, \
         failure_reason = $4, finished_at = now(), token_hash = NULL, updated_at = now() \
         WHERE id = $1 AND status IN ('running', 'pending') RETURNING pipeline_id",
    )
    .bind(job_id)
    .bind(status)
    .bind(exit_code)
    .bind(failure_reason)
    .fetch_optional(&state.db)
    .await?;
    if let Some(pid) = pipeline_id {
        state.logs.end(job_id);
        advance(state, pid).await?;
    }
    Ok(())
}

pub async fn cancel_job(state: &AppState, job: &Job) -> anyhow::Result<()> {
    match job.status.as_str() {
        "running" => {
            sqlx::query("UPDATE jobs SET cancel_requested = TRUE, updated_at = now() WHERE id = $1")
                .bind(job.id)
                .execute(&state.db)
                .await?;
        }
        "created" | "pending" | "manual" => {
            sqlx::query(
                "UPDATE jobs SET status = 'canceled', finished_at = now(), updated_at = now() \
                 WHERE id = $1 AND status IN ('created', 'pending', 'manual')",
            )
            .bind(job.id)
            .execute(&state.db)
            .await?;
            state.logs.end(job.id);
            advance(state, job.pipeline_id).await?;
        }
        _ => {}
    }
    Ok(())
}

pub async fn cancel_pipeline(state: &AppState, pipeline_id: i64) -> anyhow::Result<()> {
    let jobs = sqlx::query_as::<_, Job>(sql!(
        "SELECT {JOB_COLS} FROM jobs WHERE pipeline_id = $1 AND NOT retried AND status IN ('created','pending','manual','running')"
    ))
    .bind(pipeline_id)
    .fetch_all(&state.db)
    .await?;
    for job in &jobs {
        cancel_job(state, job).await?;
    }
    advance(state, pipeline_id).await
}

pub async fn play_job(state: &AppState, job: &Job, user: &User) -> anyhow::Result<()> {
    let n = sqlx::query(
        "UPDATE jobs SET status = 'pending', queued_at = now(), updated_at = now(), started_by = $2 \
         WHERE id = $1 AND status = 'manual'",
    )
    .bind(job.id)
    .bind(user.id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n > 0 {
        state.jobs_ready.notify_waiters();
        advance(state, job.pipeline_id).await?;
    }
    Ok(())
}

/// Clone a finished job as a new pending one; downstream skipped jobs are
/// re-evaluated once it finishes.
pub async fn retry_job(state: &AppState, job: &Job, user: &User) -> anyhow::Result<i64> {
    anyhow::ensure!(
        matches!(job.status.as_str(), "success" | "failed" | "canceled"),
        "only finished jobs can be retried"
    );
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM pipelines WHERE id = $1 FOR UPDATE").bind(job.pipeline_id).execute(&mut *tx).await?;
    let retried = sqlx::query("UPDATE jobs SET retried = TRUE WHERE id = $1 AND NOT retried")
        .bind(job.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    anyhow::ensure!(retried == 1, "job was already retried");
    let new_id: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (pipeline_id, repo_id, name, stage, stage_idx, status, \"when\", allow_failure, needs, tags, \
         resource_group, environment, spec, retry_of, started_by, queued_at) \
         SELECT pipeline_id, repo_id, name, stage, stage_idx, 'pending', \"when\", allow_failure, needs, tags, \
         resource_group, environment, spec, id, $2, now() FROM jobs WHERE id = $1 RETURNING id",
    )
    .bind(job.id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE jobs SET status = 'created', finished_at = NULL, updated_at = now() \
         WHERE pipeline_id = $1 AND NOT retried AND status = 'skipped' AND stage_idx >= $2",
    )
    .bind(job.pipeline_id)
    .bind(job.stage_idx)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    state.jobs_ready.notify_waiters();
    advance(state, job.pipeline_id).await?;
    Ok(new_id)
}

pub async fn retry_pipeline(state: &AppState, pipeline_id: i64, user: &User) -> anyhow::Result<()> {
    let jobs = sqlx::query_as::<_, Job>(sql!(
        "SELECT {JOB_COLS} FROM jobs WHERE pipeline_id = $1 AND NOT retried AND status IN ('failed','canceled') ORDER BY id"
    ))
    .bind(pipeline_id)
    .fetch_all(&state.db)
    .await?;
    for job in &jobs {
        retry_job(state, job, user).await?;
    }
    Ok(())
}

/// Background upkeep: lost runners, stale sessions.
pub async fn reaper(state: std::sync::Arc<AppState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Err(e) = reap_once(&state).await {
            tracing::warn!("reaper: {e:#}");
        }
    }
}

async fn reap_once(state: &AppState) -> anyhow::Result<()> {
    let lost: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM jobs WHERE status = 'running' AND updated_at < now() - interval '10 minutes'",
    )
    .fetch_all(&state.db)
    .await?;
    for id in lost {
        state.logs.note(id, "The runner stopped reporting for 10 minutes: job failed.").await;
        finish(state, id, "failed", None, Some("runner_system_failure")).await.context("failing lost job")?;
    }
    sqlx::query("DELETE FROM sessions WHERE expires_at < now()").execute(&state.db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, proto::Variable> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            put(&mut m, k, *v);
        }
        m
    }

    #[test]
    fn expansion() {
        let v = vars(&[("A", "1"), ("B_C", "x")]);
        assert_eq!(expand("$A-${B_C}-$MISSING-$$A", &v), "1-x--$A");
        assert_eq!(expand("price $ 5 ${", &v), "price $ 5 ${");
        assert_eq!(expand("è$A", &v), "è1");
    }
}
