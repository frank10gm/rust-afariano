//! Who may do what in a repository. Administrators maintain every repository;
//! everyone else needs a row in `repo_members`.

use crate::app::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{Job, Repo, User};

/// Ordered: each role includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Browse, clone and fetch, see pipelines and job logs.
    Read,
    /// Also push, run pipelines, run, retry and cancel jobs.
    Write,
    /// Also change settings and CI/CD variables, manage members.
    Maintain,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Read, Role::Write, Role::Maintain];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Maintain => "maintain",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Read => "Read",
            Role::Write => "Write",
            Role::Maintain => "Maintain",
        }
    }
}

/// The role `user` has in `repo_id`, if any.
pub async fn role(state: &AppState, user: &User, repo_id: i64) -> sqlx::Result<Option<Role>> {
    if user.is_admin {
        return Ok(Some(Role::Maintain));
    }
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM repo_members WHERE repo_id = $1 AND user_id = $2")
        .bind(repo_id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await?;
    Ok(role.as_deref().and_then(Role::parse))
}

/// Checks `user`'s role in `repo` against `need`. A repository the user cannot
/// see at all answers 404, so its name cannot be probed; one they can see but
/// not change answers 403.
pub async fn require(state: &AppState, user: &User, repo: &Repo, need: Role) -> AppResult<Role> {
    match role(state, user, repo.id).await? {
        Some(r) if r >= need => Ok(r),
        Some(_) => Err(AppError::Forbidden),
        None => Err(AppError::NotFound),
    }
}

/// The repository at `ns/name`, if `user` has at least `need` in it.
pub async fn repo(state: &AppState, user: &User, ns: &str, name: &str, need: Role) -> AppResult<(Repo, Role)> {
    let repo = state.repo_by_path(ns, name).await?.ok_or(AppError::NotFound)?;
    let role = require(state, user, &repo, need).await?;
    Ok((repo, role))
}

/// The repository at `ns/name` if it is the job's own: a job token reaches no other
/// (`404`, as for a repository without a role).
pub async fn job_repo(state: &AppState, job: &Job, ns: &str, name: &str) -> AppResult<Repo> {
    match state.repo_by_path(ns, name).await? {
        Some(repo) if repo.id == job.repo_id => Ok(repo),
        _ => Err(AppError::NotFound),
    }
}

/// Ids of the repositories `user` can see; `None` means all of them (administrators).
pub async fn visible_repo_ids(state: &AppState, user: &User) -> sqlx::Result<Option<Vec<i64>>> {
    if user.is_admin {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT repo_id FROM repo_members WHERE user_id = $1")
        .bind(user.id)
        .fetch_all(&state.db)
        .await
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn roles_are_ordered_and_round_trip() {
        assert!(Role::Read < Role::Write && Role::Write < Role::Maintain);
        for r in Role::ALL {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("admin"), None);
    }
}
