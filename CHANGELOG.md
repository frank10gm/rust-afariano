# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The pipeline list can show the variables each run was started with (`KEY=value`)
  in place of the commit title, for pipelines that have any; the commit hash stays,
  and the branch chip appears only when the ref is not the default branch. Per
  repository, in *Settings → General* (*Show variables in the pipeline list*, API
  `list_shows_variables`), off by default: everyone who can read the repository sees
  the values.
- A *Secret* box on each row of *Other variables* in *Run pipeline*, and
  `"secret": true` on a variable of `POST …/pipelines`. A secret value (a single line
  of 8 characters or more) shows as `••••••` in the list, is not in the page at all,
  and is masked in job logs like a masked project variable. Values of pipelines
  started before this release are hidden the same way, but not masked.

### Changed

- Repository JSON has `list_shows_variables`.

### Upgrading

Migration 5 adds `repos.list_shows_variables` (default false). Run variables are
stored with a `secret` flag; older pipelines have none, and the list never shows
their values. To roll back:
`ALTER TABLE repos DROP COLUMN list_shows_variables;
DELETE FROM _sqlx_migrations WHERE version = 5;`

## [0.6.2] - 2026-11-05

### Fixed

- Pipeline graph: the status icon in each job pill is centred with the job name; it
  sat 2.5px high, on the text baseline of its wrapper.

## [0.6.1] - 2026-11-04

### Added

- The sidebar links the project's repository on GitHub, with the running version
  (`rust-afariano 0.6.1 on GitHub`); the URL is `repository` from Cargo.toml.

### Changed

- Release workflow: the macOS runner binary is built on `macos-26`; `macos-15` runners
  never started the job for 0.6.0, whose macOS binary was built outside CI (see its
  release notes).

## [0.6.0] - 2026-11-04

### Added

- Pull mirrors, for projects whose code and `.gitlab-ci.yml` stay on GitHub,
  Bitbucket or another git server. *New repository → Mirror of*, *Settings → Mirror*
  or `PUT /api/v1/repos/{ns}/{name}/mirror` (URL: administrators). The repository
  copies the upstream's branches (all, or patterns such as `main, release/*`) and
  tags, follows its default branch, and refuses pushes over HTTPS and SSH with a
  message naming the upstream.
  - *Run pipeline* and `POST …/pipelines` fetch the upstream first, so a pipeline
    runs on the commit the upstream has now; when the upstream cannot be read,
    nothing starts and the reason is shown (`502` in the API). This replaces copy
    scripts plus a "is this still the latest commit?" check in deploy scripts.
  - Between runs: every 5 minutes (`RUST_AFARIANO_MIRROR_INTERVAL`, `0` turns it
    off), on *Sync now*, and on the upstream's webhook
    (`POST /api/v1/repos/{ns}/{name}/mirror/hook`, signed with the mirror's secret:
    GitHub, Bitbucket and GitLab formats).
  - SSH URLs use a key generated for the repository (add it upstream as a
    read-only deploy key or access key); the host key is pinned at the first
    sync. `https://` URLs take an optional token (`token` or `user:token`).
    Failed syncs say why and what to do (key not added, host key changed, token
    refused, repository not found), on the repository page and in Settings.
  - Commit statuses: with a token, every pipeline shows on its commit upstream
    (GitHub and GitHub Enterprise Cloud `*.ghe.com`: commit statuses; Bitbucket:
    build statuses), as pending, passed, failed or canceled, linking to the
    pipeline.
- Prevent outdated deployment jobs, as in GitLab (*Settings → General*, API
  `prevent_outdated_deploys`): a job with an `environment` does not run when a newer
  pipeline already deployed there successfully; it fails as
  `outdated deployment` with a note in its log. Retrying an old deployment job
  still runs it, to roll back on purpose.
- Each pipeline keeps its commit as a hidden `refs/pipelines/<id>`, so runners and
  retries still fetch it after a force push or branch deletion.

### Changed

- *Prevent outdated deployment jobs* is on for every repository after the upgrade.
  Turn it off in *Settings → General* to let an older pipeline's deployment replace
  a newer one.
- The container image includes the OpenSSH client (for SSH mirrors).
- Repository JSON has `prevent_outdated_deploys` and `mirror_url`.

### Upgrading

Migration 4 adds `repos.mirror_url`, `repos.prevent_outdated_deploys`,
`pipelines.reported_status` and the `mirrors` table. To roll back to 0.5.x, first:
`DROP TABLE mirrors; ALTER TABLE repos DROP COLUMN mirror_url, DROP COLUMN
prevent_outdated_deploys; ALTER TABLE pipelines DROP COLUMN reported_status;
DELETE FROM _sqlx_migrations WHERE version = 4;`

## [0.5.1] - 2026-11-03

### Changed

- *Run pipeline* takes the branch or tag as text, with the repository's branches and
  tags as suggestions, instead of a list. A name is a branch if one exists, else a tag,
  as in the API. Entering another existing name reloads its variables; when Run is
  clicked before that, the form shows the new ref's variables first instead of running
  with the previous ref's defaults.

## [0.5.0] - 2026-11-03

### Added

- Runner: `extra_ca_file` under `[docker]` in `config.toml` makes job and service
  containers trust one more root CA, for runners behind a TLS-inspecting proxy. The
  file is mounted in `/usr/local/share/ca-certificates`, added to the system store
  before the script runs (`update-ca-certificates`, or appended to
  `/etc/ssl/certs/ca-certificates.crt`), and `NODE_EXTRA_CA_CERTS` and `SSL_CERT_DIR`
  point at it unless the job sets them.
- Runner: several runners on one host, e.g. a docker and a shell executor on the same
  Mac. `install` and `uninstall` with a `--config` other than the default name the
  LaunchAgent, systemd unit and log after the runner
  (`io.github.frank10gm.rust-afariano-runner.<name>`,
  `rust-afariano-runner-<name>.service`); the default config keeps the plain names.
  `register --builds-dir` gives each runner its own checkouts.
- *Run pipeline* lists the top-level `variables` that have a `description`, as
  GitLab does: name, description and `value` prefilled, and a select when the
  variable has `options`. Picking another branch or tag reloads them from that
  ref's `.gitlab-ci.yml`. Other variables can still be added below.

### Changed

- A variable with `options` must have its `value` among them, and `options` must be
  a list of plain values (GitLab rejects such files too). Pipelines with one fail at
  creation with a message naming the variable.

## [0.4.1] - 2026-11-02

### Fixed

- Pushes over HTTPS larger than about 1 MB stalled behind nginx: the server sent
  its response header before reading the push, and nginx (with
  `proxy_request_buffering off`) stops forwarding the request body once it has
  passed the header on. git then waited for the rest of the pack forever and held
  the repository's push lock, so later pushes timed out too. git's answer now
  starts only once it has read the whole request.

## [0.4.0] - 2026-11-02

### Added

- The JSON API covers the rest of the web UI ([docs/API.md](docs/API.md)):
  repository settings (`GET`/`PATCH /api/v1/repos/{ns}/{name}`, and settings on
  create), members, listing and deleting CI/CD variables (never their values), job
  logs, play/retry/cancel for jobs and retry/cancel for pipelines, and for
  administrators users, tokens for service accounts and runners (list, change
  tags, untagged, paused).

### Fixed

- After an upgrade, browsers could keep the previous `app.css`, `app.js` and icons
  for up to an hour (the new eye icon showed as a blank). Their URLs now carry the
  version.

## [0.3.0] - 2026-11-02

### Added

- Maintainers and administrators can see CI/CD variable values: **Show** next to a
  hidden value (masked, file or long) loads it in place, with **Copy** and **Hide**.
  The value is fetched on demand, never embedded in the page or cached, and each
  view is logged with the user and the key. Without JavaScript it opens as plain text.

### Changed

- On narrow screens the variables table moves the environment under the key.

## [0.2.0] - 2026-11-02

### Added

- Per-repository access. Each repository's *Settings → Members* lists who can open
  it, as *read* (browse, clone, see pipelines and job logs), *write* (also push,
  run pipelines and jobs) or *maintain* (also settings, CI/CD variables and
  members). Administrators open every repository. The web UI, git over HTTP and
  SSH, and the JSON API all check it; a repository without access answers 404.

### Changed

- Only administrators create and delete repositories (web and `POST /api/v1/repos`).
- Upgrading keeps everyone's access: every existing account becomes a *maintain*
  member of every existing repository. Accounts added later start with none.

### Fixed

- The SSH server offered password authentication it always rejects, so `ssh` and
  `git` prompted for a password when no registered key matched. It now offers
  public keys only and fails with `Permission denied (publickey)`.
- The **Manage** menu on the Users page opened inside the table's box and was cut
  off, so its actions and password form could not be reached.

## [0.1.2] - 2026-11-01

### Fixed

- The account page (`/-/profile`) repeated its password form inside the page
  title, the sidebar and the breadcrumbs, breaking the layout.
- Long clone URLs no longer overflow the settings column.

## [0.1.1] - 2026-11-01

### Fixed

- The server refused to start when `RUST_AFARIANO_SECRET_KEY` (or other optional
  settings) was set to an empty string, which is what `docker-compose.yml` passes
  when you leave it unset. Empty values now mean "not set".

## [0.1.0] - 2026-11-01

First public release.

### Added

- Git hosting over SSH (embedded server, keys added in the web UI) and smart
  HTTP (personal access tokens), with a web UI to browse code, commits and
  diffs, and README rendering.
- Pipelines from a `.gitlab-ci.yml` subset: `workflow:rules`, `rules:if`,
  `only`/`except`, `stages`, `needs`, `extends` and hidden templates, `default`,
  `image`, `services` (including `docker:dind`), `variables`, `before_script`,
  `after_script`, `environment`, `resource_group`, `timeout`, `tags`,
  `when: manual`, `allow_failure`, `retry`, YAML anchors and merge keys.
  Unsupported keywords fail with a clear message.
- Pipelines start on push, by hand from the UI (with variables), or through the
  API; jobs can be retried, canceled and played; logs stream live.
- CI/CD variables per repository: file, protected, masked and
  environment-scoped, encrypted at rest.
- Runners for Linux and macOS with `docker` and `shell` executors. They only
  make outgoing HTTPS requests, so they work behind NAT; `install` sets them up
  as a systemd unit or a launchd agent.
- Local accounts with passwords, and single sign-on with any OpenID Connect
  provider.
- JSON API with personal access tokens: create repositories and runners, set
  variables, start pipelines and read their status.
- Small footprint: about 10 MB of RAM idle, under 100 MB while serving two
  clones of a 216 MB repository at once.
- Container image, `docker-compose.yml`, and prebuilt runner binaries.

[0.5.1]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.5.1
[0.5.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.5.0
[0.4.1]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.4.1
[0.4.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.4.0
[0.3.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.3.0
[0.2.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.2.0
[0.1.2]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.2
[0.1.1]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.1
[0.1.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.0
