<p align="center"><img src="docs/logo.png" width="170" alt="rust-afariano logo: an orange crab with dreadlocks"></p>

<h1 align="center">rust-afariano</h1>

<p align="center">
  <b>Self-hosted Git and GitLab-style CI in one Rust binary that idles at about 10&nbsp;MB of RAM.</b>
</p>

<p align="center">
  <a href="https://github.com/frank10gm/rust-afariano/actions/workflows/ci.yml"><img src="https://github.com/frank10gm/rust-afariano/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/frank10gm/rust-afariano/releases"><img src="https://img.shields.io/github/v/release/frank10gm/rust-afariano?sort=semver" alt="Latest release"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0"></a>
</p>

![A pipeline in rust-afariano: build, three test jobs and two deploys joined in a graph](docs/screenshots/pipeline.png)

rust-afariano does the two things most small teams use GitLab for: it **hosts Git
repositories** and **runs pipelines from `.gitlab-ci.yml`**. Nothing else. No issues,
merge requests, wiki or package registry: those are what make GitLab CE ask for
4–8 GB of memory.

It started as a replacement for a GitLab CE instance on a 1 GB cloud VM, where the
whole service had to fit in 200 MB. It now runs there, next to other apps.

## Highlights

- **One small process.** Web UI, git over SSH and HTTPS, the scheduler and the runner
  API in a single 8 MB binary, with Postgres for state.
- **Your `.gitlab-ci.yml`, mostly unchanged.** `stages`, `needs`, `rules`,
  `workflow`, `extends`, `default`, `services` (docker-in-docker included), manual
  jobs, environments, resource groups. [Full list below](#ci-compatibility).
- **Or keep your code where it is.** A repository can be a pull mirror of one on
  GitHub, Bitbucket or any git server, `.gitlab-ci.yml` included: pipe fetches it
  before every pipeline and reports results back as commit statuses.
  [More below](#code-that-lives-elsewhere-pull-mirrors).
- **Runners anywhere.** Linux or macOS, `docker` or `shell` executor. Runners only make
  outgoing HTTPS requests, so a Mac mini behind a home router works fine.
- **Proper secrets.** CI/CD variables can be file, protected, masked and
  environment-scoped, like GitLab's, and are encrypted at rest (AES-256-GCM).
- **Sign in your way.** Local accounts with passwords, single sign-on through any
  OpenID Connect provider (Keycloak, Authentik, Authelia, Zitadel, Google…), or both.
- **A UI that feels familiar.** Code browser, commits and diffs, pipeline graph, live
  job logs, statuses that update in place as jobs run, dark mode, works on a phone.

## Memory, measured

Release build, serving a real 216 MB repository. Numbers include the `git` processes
the server starts.

| Situation | Server + git |
|---|---|
| Idle | 10–13 MB |
| One full clone | 57 MB |
| Two clones at once (HTTPS + SSH) | 100 MB |
| First push of the repository, including the bitmap repack | 94 MB |

The trick is mostly git configuration: `core.packedGitLimit` and friends are set so
that `pack-objects` never maps much of a pack file at once (a full clone of the same
repository takes about 150 MB with git's defaults). Clones and repacks share two slots,
so the peak stays bounded no matter how many people fetch at once.

## Screenshots

| | |
|---|---|
| ![Repository page with files, last commit and README](docs/screenshots/repository.png) | ![Pipeline list with status badges and a mini graph per stage](docs/screenshots/pipelines.png) |
| ![Live job log with a masked secret and a file variable](docs/screenshots/job-log.png) | ![The same pipeline graph in dark mode](docs/screenshots/pipeline-dark.png) |
| ![CI/CD variables in the repository settings](docs/screenshots/variables.png) | ![Sign-in page with password and single sign-on](docs/screenshots/sign-in.png) |

## Quick start

You need Docker with Compose.

```sh
curl -fsSLO https://raw.githubusercontent.com/frank10gm/rust-afariano/main/docker-compose.yml
echo "POSTGRES_PASSWORD=$(openssl rand -hex 16)" > .env
docker compose up -d
docker compose logs rust-afariano | grep setup
```

Open the setup link from the log to create the first administrator, then:

1. **Create a repository** and push to it. Over HTTPS, use a personal access token
   (*Your account → Personal access tokens*) as the password. Over SSH, add your key
   on the same page.
2. **Add a runner** under *Runners → New runner*, then on the machine that should run
   jobs:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/frank10gm/rust-afariano/main/contrib/install-runner.sh | sh
   rust-afariano-runner register --url http://your-server:8080 --token rfr_… --executor docker
   rust-afariano-runner install    # systemd unit on Linux (as root), LaunchAgent on macOS
   ```

   Add `--privileged` for docker-in-docker services, or use `--executor shell` to run
   jobs directly on the host. For both on one machine, register the second runner
   with its own `--config` and `--builds-dir`, then `--config … install` it: its
   service is named after it. Behind a TLS-inspecting proxy, set
   `extra_ca_file = "/abs/path/ca.crt"` under `[docker]` in the runner's
   `config.toml`: jobs and services then trust that CA (system store, Node, Go).
3. **Push a `.gitlab-ci.yml`.** Pipelines start on push, from *Run pipeline* in the UI
   (with variables), or through the [API](#api).

For a public server, set `RUST_AFARIANO_PUBLIC_URL` and put a TLS reverse proxy in
front of port 8080. [`contrib/nginx.conf.example`](contrib/nginx.conf.example) has the
streaming settings git and live logs need. Port 2222 (git over SSH) is served
directly.

## Code that lives elsewhere (pull mirrors)

When a project's code and its `.gitlab-ci.yml` stay on GitHub, Bitbucket or another
git server, make its repository here a **mirror** (*New repository → Mirror of*, or
*Settings → Mirror*): rust-afariano copies it and runs its pipelines, and refuses
pushes, so the copy never holds a commit the upstream lacks.

- **No stale pipelines.** *Run pipeline* (and the API) fetches the upstream first,
  so a pipeline always runs on the commit the upstream has now. Between runs, pipe
  checks every 5 minutes (`RUST_AFARIANO_MIRROR_INTERVAL`), at once when the
  upstream's webhook calls, or on *Sync now*.
- **Access, read-only.** For an SSH URL (`git@bitbucket.org:team/app.git`), pipe makes
  a key for this repository alone: add it on GitHub as a *deploy key* (write access
  off) or on Bitbucket as an *access key*. For an `https://` URL, give it a token
  that may read the repository, or none for a public one. The host key is pinned
  at the first sync.
- **Webhook (optional).** *Settings → Mirror* shows its URL and secret; GitHub,
  Bitbucket and GitLab webhooks are understood. Only the signature is checked:
  the call just starts a sync.
- **Commit statuses (optional).** With a token (GitHub: *Commit statuses: Read and
  write*; Bitbucket: a repository access token with *Repositories: Read*), every
  pipeline shows on its commit and pull request upstream as pending, passed or
  failed.
- **Branches** to copy can be limited (`main, release/*`); every tag is copied.
  Pipelines on push run when a sync brings new commits, if *Create pipelines on
  push* is on (off for new mirrors); the first sync starts none.

Pipelines keep their commit after the upstream rewrites a branch
(`refs/pipelines/<id>`, hidden from clients), so retries still work.

## CI compatibility

rust-afariano reads `.gitlab-ci.yml`. Pipelines that use something it does not
support fail at creation with a message naming the job and the keyword, instead of
silently doing something else.

| Supported | Not supported (yet) |
|---|---|
| `stages` (with `.pre` / `.post`), `needs`, `workflow:rules` | `include` |
| `rules: if / when / allow_failure / variables` | `rules: changes / exists` |
| `only` / `except` (refs, `branches`, `tags`, regex) | `artifacts`, `cache`, `dependencies` |
| `extends`, hidden jobs, YAML anchors and `<<` merge keys | `parallel`, `trigger`, child pipelines |
| `default`, `image` (with `entrypoint`), `services` (alias, command, variables) | `pages`, `release`, `secrets` |
| `before_script`, `script`, `after_script` | `when: delayed` |
| `variables` (global, job, project, per run) with `$VAR` expansion | Scheduled pipelines |
| `variables` with `description` / `options`: prefilled on *Run pipeline* | |
| `when: on_success / on_failure / always / manual / never` | |
| `allow_failure`, `timeout`, `tags` | `retry` and `interruptible` are accepted but have no effect yet |
| `environment` (name, url), `resource_group`, GitLab's *Prevent outdated deployment jobs* | |
| Predefined `CI_*` variables, `CI_JOB_TOKEN`, `CI_REPOSITORY_URL` | |

Rule expressions support `==`, `!=`, `=~`, `!~`, `&&`, `||`, `!`, parentheses,
`null` and `/regex/i`.

## Signing in

| Method | How to enable |
|---|---|
| Passwords | On by default (`RUST_AFARIANO_PASSWORD_LOGIN=true`). Administrators create accounts under *Users*; the very first admin comes from the setup link. |
| OpenID Connect | Set `RUST_AFARIANO_OIDC_ISSUER`, `RUST_AFARIANO_OIDC_CLIENT_ID` and, for a confidential client, `RUST_AFARIANO_OIDC_CLIENT_SECRET`. Register `<public url>/auth/oidc/callback` as redirect URI. Authorization code + PKCE, with nonce, issuer and audience checks. |
| Who gets in through SSO | People with an existing account (same email). `RUST_AFARIANO_ALLOWED_EMAILS` also lets new people in on their first sign-in, as exact emails or `*@yourcompany.com`. |

People see only the repositories they are members of (*Settings → Members* of each
repository), with *read*, *write* (push, run pipelines) or *maintain* (settings,
variables, members) access. Administrators see every repository, create and delete
repositories, and manage users and runners. See [SECURITY.md](SECURITY.md) for the
security model.

## Configuration

All settings are environment variables (or the same-named flags, see
`rust-afariano --help`).

| Variable | Default | |
|---|---|---|
| `RUST_AFARIANO_DATABASE_URL` | required | Postgres connection URL |
| `RUST_AFARIANO_PUBLIC_URL` | required | Address people use, e.g. `https://git.example.com` |
| `RUST_AFARIANO_PASSWORD_LOGIN` | `true` | Sign in with email and password |
| `RUST_AFARIANO_OIDC_ISSUER` | | OpenID Connect issuer URL |
| `RUST_AFARIANO_OIDC_CLIENT_ID` | | |
| `RUST_AFARIANO_OIDC_CLIENT_SECRET` | | Empty for a public client |
| `RUST_AFARIANO_OIDC_NAME` | `single sign-on` | Label of the sign-in button |
| `RUST_AFARIANO_ALLOWED_EMAILS` | | Emails or `*@domain` that get an account on first SSO sign-in |
| `RUST_AFARIANO_SECRET_KEY` | generated | 32 bytes, base64. Encrypts CI/CD variables. If unset, a key is created in the data directory: back it up together with the database |
| `RUST_AFARIANO_DATA_DIR` | `./data` | Repositories, job logs, SSH host key |
| `RUST_AFARIANO_HTTP_ADDR` | `127.0.0.1:8080` | `0.0.0.0:8080` in the container image |
| `RUST_AFARIANO_SSH_ADDR` | `0.0.0.0:2222` | Empty disables SSH |
| `RUST_AFARIANO_SSH_PUBLIC_PORT` | `2222` | Port shown in clone URLs |
| `RUST_AFARIANO_MAX_PACKS` | `2` | Clones and repacks allowed at the same time |
| `RUST_AFARIANO_MIRROR_INTERVAL` | `300` | Seconds between two checks of a pull mirror's upstream; `0` turns polling off (webhooks and *Run pipeline* still fetch) |
| `RUST_AFARIANO_SESSION_TTL_HOURS` | `720` | |
| `RUST_AFARIANO_LOG` | `info` | Log filter, e.g. `debug,sqlx=warn` |

## API

Personal access tokens (`Authorization: Bearer rfp_…`) work with a JSON API that
covers what the web UI does: repositories and their settings, members, CI/CD
variables, pipelines and jobs (logs, play, retry, cancel) and, for administrators,
users, service-account tokens and runners. Reference: [docs/API.md](docs/API.md).

```sh
# Start a pipeline, with variables
curl -X POST https://git.example.com/api/v1/repos/acme/website/pipelines \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"ref": "main", "variables": [{"key": "DEPLOY", "value": "1"}]}'

# Its status and jobs
curl -H "Authorization: Bearer $TOKEN" https://git.example.com/api/v1/repos/acme/website/pipelines/12
```

[`contrib/import-repo.sh`](contrib/import-repo.sh) uses it to move a repository
over from elsewhere.

## How it compares

| | rust-afariano | GitLab CE | Forgejo + Actions | Woodpecker |
|---|---|---|---|---|
| Git hosting and web UI | Yes | Yes | Yes | No (needs a forge) |
| Pipeline format | `.gitlab-ci.yml` subset | `.gitlab-ci.yml` | GitHub Actions | Its own YAML |
| Issues, pull/merge requests | No | Yes | Yes | No |
| Memory | ~10 MB idle (measured above) | 4 GB minimum (official docs) | Low hundreds of MB | Server and agents, plus a forge |

If you need code review, issues or a package registry, use Forgejo or GitLab. If you
want your repositories and your `.gitlab-ci.yml` pipelines on a small machine,
rust-afariano is built for that.

## Building from source

Rust 1.88 or newer, `git`, and a Postgres database.

```sh
cp .env.example .env            # point it at your database
set -a; . ./.env; set +a
cargo run -p rust-afariano      # http://localhost:8080, setup link in the log
cargo test --workspace
```

| Crate | |
|---|---|
| [`crates/server`](crates/server) | `rust-afariano`: web UI (axum + askama), git over SSH (russh) and smart HTTP, JSON and runner APIs, scheduler |
| [`crates/runner`](crates/runner) | `rust-afariano-runner`: runs jobs with Docker or a shell |
| [`crates/ci`](crates/ci) | `.gitlab-ci.yml` to pipeline plan: pure and thoroughly tested |
| [`crates/proto`](crates/proto) | Types shared by the server and the runner |

Architecture and CI semantics are described in [docs/DESIGN.md](docs/DESIGN.md).
Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT),
at your option. The bundled IBM Plex fonts are under the
[SIL Open Font License](crates/server/static/fonts/OFL.txt).

The name is a pun on Rust and Rastafarian; the crab keeps the vibe relaxed.
