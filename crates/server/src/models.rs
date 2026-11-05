use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx::types::Json;

use crate::util::glob_match;

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub name: String,
    /// Selected only where it matters (sessions, tokens); false elsewhere.
    #[sqlx(default)]
    pub is_admin: bool,
}

impl User {
    pub fn display(&self) -> &str {
        if self.name.is_empty() { &self.email } else { &self.name }
    }

    pub fn initial(&self) -> String {
        crate::web::initial(self.display())
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Repo {
    pub id: i64,
    pub namespace: String,
    pub name: String,
    pub description: String,
    pub default_branch: String,
    pub protected_branches: String,
    pub pipelines_on_push: bool,
    pub created_at: DateTime<Utc>,
    pub pushed_at: Option<DateTime<Utc>>,
    /// Set for a pull mirror: the upstream it copies. Pushes to it are refused.
    pub mirror_url: Option<String>,
    /// Jobs with an environment do not run when a newer pipeline already deployed there.
    pub prevent_outdated_deploys: bool,
    /// The pipeline list shows each run's variables instead of the commit title.
    pub list_shows_variables: bool,
}

pub const REPO_COLS: &str = "id, namespace, name, description, default_branch, protected_branches, pipelines_on_push, \
     created_at, pushed_at, mirror_url, prevent_outdated_deploys, list_shows_variables";

impl Repo {
    pub fn is_mirror(&self) -> bool {
        self.mirror_url.is_some()
    }

    pub fn path(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    pub fn is_protected(&self, ref_name: &str, is_tag: bool) -> bool {
        // Tags are protected like GitLab's default "protected tags: *" would make them.
        is_tag
            || self
                .protected_branches
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .any(|p| glob_match(p, ref_name))
    }
}

/// A variable given when starting a pipeline by hand or through the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineVar {
    pub key: String,
    pub value: String,
    /// `Some(true)`: hidden in the pipeline list and masked in job logs. `None` in API
    /// calls without it (`clean_vars` makes it `Some(false)`) and in pipelines stored before
    /// the flag existed, whose values the list never shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<bool>,
}

impl PipelineVar {
    /// Marked secret: masked in job logs, hidden in the pipeline list.
    pub fn is_secret(&self) -> bool {
        self.secret == Some(true)
    }

    /// The pipeline list may show the value: only when the run said it is not secret.
    pub fn value_listed(&self) -> bool {
        self.secret == Some(false)
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Pipeline {
    pub id: i64,
    pub repo_id: i64,
    pub iid: i32,
    pub ref_name: String,
    pub ref_is_tag: bool,
    pub sha: String,
    pub before_sha: String,
    pub source: String,
    pub status: String,
    pub variables: Json<Vec<PipelineVar>>,
    pub stages: Vec<String>,
    pub yaml_vars: Json<Vec<(String, String)>>,
    pub error: Option<String>,
    pub commit_title: String,
    pub created_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub const PIPELINE_COLS: &str = "id, repo_id, iid, ref_name, ref_is_tag, sha, before_sha, source, status, variables, stages, \
     yaml_vars, error, commit_title, created_by, created_at, started_at, finished_at";

#[allow(dead_code)] // mirrors the table row
#[derive(Debug, Clone, FromRow)]
pub struct Job {
    pub id: i64,
    pub pipeline_id: i64,
    pub repo_id: i64,
    pub name: String,
    pub stage: String,
    pub stage_idx: i32,
    pub status: String,
    pub when: String,
    pub allow_failure: bool,
    pub needs: Option<Vec<String>>,
    pub tags: Vec<String>,
    pub resource_group: Option<String>,
    pub environment: Option<String>,
    pub spec: Json<rust_afariano_ci::Job>,
    pub runner_id: Option<i64>,
    pub cancel_requested: bool,
    pub exit_code: Option<i32>,
    pub failure_reason: Option<String>,
    pub log_size: i64,
    pub retried: bool,
    pub started_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub queued_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

pub const JOB_COLS: &str = "id, pipeline_id, repo_id, name, stage, stage_idx, status, \"when\", allow_failure, needs, tags, \
     resource_group, environment, spec, runner_id, cancel_requested, exit_code, failure_reason, log_size, retried, \
     started_by, created_at, queued_at, started_at, finished_at, updated_at";

impl Job {
    pub fn is_terminal(&self) -> bool {
        is_terminal(&self.status)
    }

    pub fn duration_secs(&self) -> Option<i64> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Utc::now);
        Some((end - start).num_seconds())
    }
}

pub fn is_terminal(status: &str) -> bool {
    matches!(status, "success" | "failed" | "canceled" | "skipped")
}

#[allow(dead_code)] // mirrors the table row
#[derive(Debug, Clone, FromRow)]
pub struct Variable {
    pub id: i64,
    pub repo_id: i64,
    pub key: String,
    pub value: String,
    pub file: bool,
    pub protected: bool,
    pub masked: bool,
    pub environment_scope: String,
}

#[allow(dead_code)] // mirrors the table row
#[derive(Debug, Clone, FromRow)]
pub struct Runner {
    pub id: i64,
    pub name: String,
    pub tags: Vec<String>,
    pub run_untagged: bool,
    pub paused: bool,
    pub created_at: DateTime<Utc>,
    pub contacted_at: Option<DateTime<Utc>>,
    pub version: String,
    pub platform: String,
    pub executor: String,
    pub hostname: String,
}

pub const RUNNER_COLS: &str =
    "id, name, tags, run_untagged, paused, created_at, contacted_at, version, platform, executor, hostname";

impl Runner {
    pub fn online(&self) -> bool {
        self.contacted_at.is_some_and(|t| (Utc::now() - t).num_seconds() < 120)
    }
}
