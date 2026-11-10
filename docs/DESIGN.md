# rust-afariano design

Git hosting + CI pipelines, nothing else. Target: the server stays under
**200 MB RAM** (container `--memory 200m`) on a 1 GB VM; jobs run elsewhere.

```
 browser ──HTTPS──┐                         ┌── runner (Mac, Linux…) polls ──┐
 git CLI ──HTTPS──┤  nginx-core :443 ──▶ rust-afariano :8080             │
 git CLI ──SSH :2222──────────────────▶ rust-afariano :2222             │
                                          │   Postgres                     │
                                          │   data dir: repos/ logs/       │
                                          └──────── /api/v1/runner/* ◀────┘
```

## Crates

| Crate | What |
|---|---|
| `rust-afariano-proto` | runner ⇄ server JSON types (`crates/proto/src/lib.rs` is the contract) |
| `rust-afariano-ci` | `.gitlab-ci.yml` subset → pipeline plan. Pure, no I/O |
| `rust-afariano` | axum web UI + git smart HTTP + russh SSH + runner API + scheduler |
| `rust-afariano-runner` | polls the server, runs jobs with `docker` or `shell` executor |

## Auth

- **Passwords**: Argon2id (19 MiB, 2 passes; at most two hashes at once), 5 failed
  attempts per email per 15 minutes, a dummy hash for unknown emails. The first
  administrator is created through a one-time setup link printed at startup while
  there are no accounts.
- **OpenID Connect**: discovery, authorization code + PKCE (S256) + `state` +
  `nonce`. The ID token comes straight from the token endpoint over TLS, so per
  OIDC Core §3.1.3.7 its signature is not checked; `iss`, `aud`, `exp` and `nonce`
  are. `email_verified: false` is refused. Client authentication:
  `client_secret_basic`, or `client_secret_post` when that is all the provider offers.
- **Rich auth servers** (auth.hartwellbridge.com and compatible): a non-standard
  PKCE login (`/api/request-login`, `/auth/token` with `code_verifier`, a `user`
  object in the answer). Enabled with `RUST_AFARIANO_RICH_AUTH_URL`.
- SSO identities map to accounts by email. Unknown emails get an account only when
  `RUST_AFARIANO_ALLOWED_EMAILS` matches (exact or `*@domain`); the first account
  ever created is an administrator.
- Sessions are random tokens stored hashed in Postgres (30 days by default);
  cookies are `HttpOnly`, `SameSite=Lax`, `Secure` on HTTPS. Cookie-authenticated
  POSTs must carry our own `Origin` (or `Referer`).
- git over HTTPS: basic auth, password = personal access token (`rfp_…`), or job
  token (`rfj_…`, user `rust-afariano-ci`, read-only, only the job's repo, only
  while it runs). Without valid credentials every repository path answers 401.
- git over SSH: public keys added in the UI. Disabled accounts lose web, token and
  SSH access at once.
- Repository access: `repo_members (repo_id, user_id, role)` with `read` < `write` <
  `maintain`; administrators count as `maintain` everywhere without a row. Every
  web page, git service (HTTP and SSH) and API call checks it (`access.rs`). No
  role answers 404, like a missing repository, so names cannot be probed; too low
  a role answers 403. Pushing needs `write`, settings, variables and members
  `maintain`; creating and deleting repositories is for administrators.
- Runners: `rfr_…` token shown once when the runner is created (admins only).
- All tokens are stored as SHA-256. CI/CD variable values are stored encrypted
  with AES-256-GCM (`enc:v1:` + base64(nonce ‖ ciphertext)).

## CI subset (`rust-afariano-ci`)

Supported keywords — exactly what the existing pipelines use, plus a few cheap ones:

- top level: `workflow: {rules}`, `stages`, `variables`, `default`
  (`image`, `services`, `before_script`, `after_script`, `tags`, `timeout`,
  `interruptible`), and the legacy top-level `image`, `services`,
  `before_script`, `after_script`.
- hidden jobs (`.name`) and `extends` (string or list, recursive, deep-merge of
  maps, later wins, arrays replaced — GitLab semantics); YAML anchors/aliases
  and `<<` merge keys.
- job: `stage` (default `test`), `image` (string or `{name, entrypoint}`),
  `services` (string or `{name, alias, entrypoint, command, variables}`),
  `before_script`/`script`/`after_script` (string or list, nested lists
  flattened), `variables` (scalar or `{value, description, expand}`), `needs`
  (strings or `{job, optional}`), `rules` (`if`, `when`, `allow_failure`,
  `variables`; `changes`/`exists` → error "not supported"), `only`/`except`
  (refs list incl. `branches`, `tags`, `/regex/`), `when` (`on_success`,
  `on_failure`, `always`, `manual`, `never`), `allow_failure` (bool or
  `{exit_codes}` → bool true), `environment` (string or `{name, url}`),
  `resource_group`, `timeout`, `tags`, `interruptible`, `retry` (int or `{max}`).
- unsupported keys that change behaviour (`include`, `trigger`, `parallel`,
  `artifacts`, `cache`, …) fail the pipeline with a clear message; harmless ones
  (`coverage`, `description`) are ignored.
- `stages` default `[build, test, deploy]`; `.pre` first and `.post` last always exist.
- `rules:if` grammar: `$VAR`, `"str"`/`'str'`, `null`, `/re/flags`, `==`, `!=`,
  `=~`, `!~`, `&&`, `||`, `!`, parentheses. Undefined var = null; `$VAR` alone =
  defined and non-empty. Regex = Rust `regex` (RE2 syntax), flag `i` supported.
- Rule evaluation sees: predefined vars + project/pipeline variables (given by
  the caller) + global YAML variables + the job's YAML variables (caller vars win).
- `workflow:rules`: first match decides; no match → no pipeline; `when: never`
  → no pipeline. No `workflow` → pipeline runs.
- Job `rules`: first matching rule applies its `when` (default `on_success`),
  `allow_failure`, `variables`; no match → job not added. `when: manual` from
  rules defaults `allow_failure: false`; a job-level `when: manual` defaults
  `allow_failure: true` (GitLab semantics).
- Timeouts: `30m`, `1h`, `1h 30m`, `90 minutes`, `2 hours`, `3600` (seconds).

## Scheduling (server)

Job statuses: `created` (waiting on deps) → `pending` (runner may take it) →
`running` → `success`/`failed`/`canceled`; also `skipped`, `manual`.

- deps = `needs` jobs if `needs` is given, else every job in earlier stages.
- deps done when all are terminal, or `manual` with `allow_failure`.
- deps ok when none `failed` without `allow_failure`/`canceled`.
- `on_success`: ok → pending, else skipped. `on_failure`: not ok → pending,
  else skipped. `always` → pending. `manual`: ok → manual, else skipped.
- `resource_group`: at most one running job per (repo, group).
- Assignment: one Postgres advisory lock, job tags ⊆ runner tags, untagged jobs
  only to `run_untagged` runners. Runners long-poll; a `tokio::Notify` wakes them.
- Lost runner: a running job with no log/heartbeat for 10 min fails
  (`runner_system_failure`).

- Prevent outdated deployment jobs (per repository, on by default, like GitLab):
  when a runner would take a job with an `environment`, and a job of a newer
  pipeline already succeeded on that environment, the job fails with
  `outdated_deployment` and a note in its log instead. Retried jobs are exempt
  (rollback); environment names with `$` (review apps) are not compared.
- Each pipeline's commit is kept as `refs/pipelines/<id>` (`transfer.hideRefs`:
  never advertised, never pushable; `uploadpack.allowTipSHA1InWant`), so runners
  and retries still fetch it after a force push.

## Live pages (`web/live.rs`, `app.js`)

The home page, the pipeline list, a pipeline and a job update in place, without
polling. Every job change ends in `pipelines::advance` (or `create`), which, once
committed, bumps a change counter and sends `(repo, pipeline)` on a
`tokio::sync::broadcast` channel. Single process, so no Postgres `LISTEN` needed.

- Streams are server-sent events, the same as the job log: one way is all a page
  needs, reconnects are plain GETs, and the nginx settings for logs already fit.
  `GET /{ns}/{name}/-/pipelines/events[?pipeline=<iid>]` (read access) and
  `GET /-/events` (repositories the user sees) send `change` with no data; the
  page fetches its own URL again, so the usual access checks apply, and swaps the
  `data-live-region` elements that differ. A different `data-live-key` (a manual
  job that started) reloads the page.
- Missed changes: a page carries the counter read before its data was loaded
  (`data-live-seq`); a stream opened with another `?since=` pings at once. The
  counter starts at the clock, so a page from before a restart is always behind.
  The page reconnects itself (backoff 1 s to 60 s) with the counter of what it
  shows, and drops its stream while hidden: over HTTP/1.1 browsers allow six
  connections per host.
- A job page keeps one connection: its log stream also carries the pipeline's
  changes, and the pipeline stream opens when the log ends.
- Running durations count in the page (`data-elapsed`), between changes.

## Pull mirrors (`mirror.rs`)

A repository with `mirror_url` copies an upstream; its `mirrors` row holds the
generated ed25519 key (private half encrypted), pinned `known_hosts`, an optional
HTTPS token, branch patterns, the webhook secret, an optional commit status token
and the outcome of the last sync, webhook call and status update.

- A sync holds the repository's push lock: `git ls-remote --symref` (patterns
  `HEAD refs/heads/* refs/tags/*`, so GitHub's `refs/pull/*` stay out), then, only
  if a wanted branch or tag differs, `git fetch --prune` with one refspec per
  pattern plus all tags, under a pack slot. Branches no pattern covers any more are
  deleted; HEAD follows the upstream's. Then the same path as a push
  (`push::after_ref_update`, no user): ref diff, pipelines on push, repack.
  The first sync of an upstream starts no pipelines.
- git runs with `GIT_ALLOW_PROTOCOL=https:ssh`, no askpass, no credential helper,
  `GIT_SSH_COMMAND` with `-F /dev/null`, the key and known_hosts written to a
  0700 temporary directory in the data dir (removed after, and at startup), and
  `StrictHostKeyChecking=accept-new` only while no host key is pinned. HTTPS
  tokens go in an `http.extraHeader` from the environment.
- Triggers: `Run pipeline` and `POST …/pipelines` (synchronous, up to 2 minutes,
  refuse to start on failure), the poller (every `RUST_AFARIANO_MIRROR_INTERVAL`
  seconds per mirror, one at a time), webhooks and *Sync now* (queued in the
  background; webhooks for a mirror already queued coalesce).
- Pushes to a mirror are refused over HTTP (`403`, text git prints as
  `remote: …`) and SSH.
- Commit statuses (`commit_status.rs`): pipeline creation and every pipeline
  status change queue the pipeline id; one worker reads the current status, skips
  it if already reported (`pipelines.reported_status`), and posts it to GitHub
  (`/repos/{owner}/{repo}/statuses/{sha}`, context = server host) or Bitbucket
  (`/2.0/repositories/{ws}/{slug}/commit/{sha}/statuses/build`, key = server host).

## Runner

- Config: `~/.config/rust-afariano-runner/config.toml` (or `/etc/rust-afariano-runner/config.toml`
  as root): `url`, `token`, `executor`, `concurrent`, `builds_dir`,
  `[docker] default_image, privileged, volumes, pull_policy`.
- `register --url --token` (verifies and writes config), `run`, `install`
  (launchd LaunchAgent on macOS so Docker Desktop's socket is reachable; systemd
  unit on Linux), `uninstall`.
- Job steps: prepare build dir → git fetch/checkout at SHA (host git, job token)
  → (docker) network + services + wait for service ports → job container
  (image entrypoint kept unless overridden) running `sh -c`-style generated
  script from a mounted file, stdin `/dev/null` → `after_script` → cleanup.
- Script: `set -e`, each line echoed green as `$ cmd` like GitLab.
- Docker: per-job network `rust-afariano-job-<id>`; services get the job's variables,
  aliases from the image name (`docker:29-dind` → `docker`;
  `reg.io/a/b:1` → `reg.io__a__b` and `reg.io-a-b`) plus `alias`; config
  `volumes` (default `["/certs/client"]`) become per-job volumes mounted into
  job and services; the build dir is bind-mounted at `/builds/<project_path>`
  in job and services; `privileged` applies to both.
- File variables written to a per-job temp dir mounted at `/rust-afariano/files`.
- Logs streamed every ~1 s (`PUT …/log?offset=`), masked values replaced with
  `[MASKED]` (carry-over buffer so a secret split across chunks is still masked).
- Timeout and cancel kill the containers/process group.
