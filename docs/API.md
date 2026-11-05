# JSON API

Every call takes a personal access token: `Authorization: Bearer rfp_…`. Create
one under *SSH keys and tokens* in the web UI, or, as an administrator, for a
service account with `POST /api/v1/users/{user}/tokens`.

The token acts as its account, with the same rules as the web UI:

- **Repository** calls need a role in that repository: *read*, *write* or
  *maintain* (administrators have *maintain* everywhere). Without any role the
  answer is `404`, like a missing repository; with too low a role it is `403`.
- **User and runner** calls, and creating repositories, are for administrators.

Bodies are JSON (`Content-Type: application/json`). Errors are
`{"message": "…"}` with a `4xx`/`5xx` status. `{user}` in a path is an account id
or its email.

```sh
API=https://git.example.com/api/v1
auth=(-H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json")
```

## Repositories

| Call | Role | Body / answer |
|---|---|---|
| `POST /repos` | admin | `{"namespace", "name", "description"?, "default_branch"?, "protected_branches"?: [..], "pipelines_on_push"?, "mirror"?}` → `201` repository; `mirror` as in `PUT …/mirror` below |
| `GET /repos/{ns}/{name}` | read | repository |
| `PATCH /repos/{ns}/{name}` | maintain | any of `description`, `default_branch`, `protected_branches` (list, replaces), `pipelines_on_push`, `prevent_outdated_deploys`, `list_shows_variables` → repository |

A repository is `{"path", "description", "default_branch", "protected_branches",
"pipelines_on_push", "prevent_outdated_deploys", "list_shows_variables", "mirror_url", "role", "web_url",
"http_url", "ssh_url"}`, where `role` is the caller's and `mirror_url` is `null`
unless the repository is a pull mirror.

```sh
curl "${auth[@]}" -X POST $API/repos -d '{"namespace": "acme", "name": "site",
  "protected_branches": ["main", "staging"], "pipelines_on_push": false}'
```

## Pull mirrors

A mirror copies a repository from another server and refuses pushes (README,
*Code that lives elsewhere*). On a mirror, `POST …/pipelines` fetches the upstream
first; if that fails, nothing starts and the answer is `502`.

| Call | Role | Body / answer |
|---|---|---|
| `GET /repos/{ns}/{name}/mirror` | maintain | mirror |
| `PUT /repos/{ns}/{name}/mirror` | maintain (admin to set or change `url`) | `{"url", "branches"?, "https_token"?, "status_token"?}` → mirror. Makes the repository a mirror, or changes its settings. A token left out stays; `""` removes it |
| `DELETE /repos/{ns}/{name}/mirror` | admin | `204`: an ordinary repository again, its content kept |
| `POST /repos/{ns}/{name}/mirror/sync` | write | fetches now → `{"changed", "synced_at"}`, or `502 {"message"}` with git's error and a hint |
| `POST /repos/{ns}/{name}/mirror/secret` | maintain | a new webhook secret → `{"webhook_secret"}` |
| `POST /repos/{ns}/{name}/mirror/hook` | none: signed | the upstream's webhook → `202`. Needs GitHub's `X-Hub-Signature-256` or Bitbucket's `X-Hub-Signature` (`sha256=` HMAC of the body with the secret) or GitLab's `X-Gitlab-Token`; every refusal is `404` |

A mirror is `{"url", "upstream", "ssh_public_key", "host_keys", "branches",
"has_https_token", "synced_at", "checked_at", "error", "webhook_url",
"webhook_secret", "hook_at", "hook_error", "forge", "has_status_token",
"status_at", "status_error"}`. `ssh_public_key` (SSH URLs only) goes on the
upstream as a read-only deploy key or access key; `forge` is `GitHub`, `Bitbucket`
or `null` (no commit statuses); `error`, `hook_error` and `status_error` say why
the last sync, webhook call or status update failed.

```sh
curl "${auth[@]}" -X POST $API/repos -d '{"namespace": "devops", "name": "app",
  "mirror": {"url": "git@bitbucket.org:team/app.git", "branches": "main, release/*"}}'
curl -s "${auth[@]}" $API/repos/devops/app/mirror | jq -r .ssh_public_key   # add it on Bitbucket
curl "${auth[@]}" -X POST $API/repos/devops/app/mirror/sync
```

## Members

| Call | Role | Body / answer |
|---|---|---|
| `GET /repos/{ns}/{name}/members` | maintain | `[{"user_id", "email", "name", "role", "admin"}]` |
| `PUT /repos/{ns}/{name}/members/{user}` | maintain | `{"role": "read" \| "write" \| "maintain"}` → member (adds, or changes the role) |
| `DELETE /repos/{ns}/{name}/members/{user}` | maintain | `204`, `404` if not a member |

## CI/CD variables

| Call | Role | Body / answer |
|---|---|---|
| `GET /repos/{ns}/{name}/variables` | maintain | `[{"key", "environment_scope", "file", "protected", "masked"}]`, never values |
| `PUT /repos/{ns}/{name}/variables/{key}` | maintain | `{"value", "file"?, "protected"?, "masked"?, "environment_scope"?}` → `204` (creates, or replaces the same key and scope) |
| `DELETE /repos/{ns}/{name}/variables/{key}?environment_scope=…` | maintain | `204`; scope defaults to `*` |

Values cannot be read back through the API; maintainers can see them in the web UI.

## Pipelines and jobs

| Call | Role | Body / answer |
|---|---|---|
| `POST /repos/{ns}/{name}/pipelines` | write | `{"ref", "variables"?: [{"key", "value", "secret"?}]}` → `201 {"iid", "web_url"}`; `"secret": true` (a single line of 8 characters or more) hides the value in the web pipeline list and masks it in job logs |
| `GET /repos/{ns}/{name}/pipelines/{iid}` | read | pipeline |
| `POST /repos/{ns}/{name}/pipelines/{iid}/retry` | write | retries failed and canceled jobs → pipeline |
| `POST /repos/{ns}/{name}/pipelines/{iid}/cancel` | write | → pipeline |
| `GET /repos/{ns}/{name}/jobs/{id}/log` | read | plain text, colours removed; header `X-Job-Status` |
| `POST /repos/{ns}/{name}/jobs/{id}/play` | write | starts a manual job → job |
| `POST /repos/{ns}/{name}/jobs/{id}/retry` | write | → the new job |
| `POST /repos/{ns}/{name}/jobs/{id}/cancel` | write | → job |

A pipeline is `{"iid", "status", "ref", "sha", "source", "error", "web_url",
"jobs": [{"id", "name", "stage", "status", "when", "environment",
"failure_reason", "web_url"}]}`; a job answer is `{"id", "name", "status", "web_url"}`.

```sh
iid=$(curl -s "${auth[@]}" -X POST $API/repos/acme/site/pipelines -d '{"ref": "staging"}' | jq .iid)
curl -s "${auth[@]}" $API/repos/acme/site/pipelines/$iid | jq '.status, .jobs'
curl -s "${auth[@]}" $API/repos/acme/site/jobs/42/log
```

## Users (administrators)

| Call | Body / answer |
|---|---|
| `GET /users` | `[{"id", "email", "name", "admin", "disabled"}]` |
| `POST /users` | `{"email", "name"?, "password"?, "admin"?}` → `201` user. Without a password the account signs in through SSO only, or not at all (a service account) |
| `POST /users/{user}/tokens` | `{"name", "expires_days"?}` → `201 {"user_id", "email", "name", "token"}`, the token shown once |

Tokens for another administrator are refused: they create their own. Each token
created for someone else is logged (`access token created for another account`).

A service account for a sync job, with access to one repository only:

```sh
curl "${auth[@]}" -X POST $API/users -d '{"email": "sync@example.com", "name": "Sync"}'
curl "${auth[@]}" -X PUT $API/repos/acme/site/members/sync@example.com -d '{"role": "write"}'
curl "${auth[@]}" -X POST $API/users/sync@example.com/tokens -d '{"name": "sync job"}'
```

## Runners (administrators)

| Call | Body / answer |
|---|---|
| `GET /runners` | `[runner]` |
| `POST /runners` | `{"name", "tags"?, "run_untagged"?}` → `201 {"id", "token"}`, the token shown once |
| `PATCH /runners/{id}` | any of `tags` (replaces the list), `run_untagged`, `paused` → runner |

A runner is `{"id", "name", "tags", "run_untagged", "paused", "online",
"contacted_at", "hostname", "platform", "executor", "version"}`.
