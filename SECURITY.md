# Security

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub:
**Security → Report a vulnerability** on
https://github.com/frank10gm/rust-afariano/security/advisories/new.
Do not open a public issue for them.

Include what you found, how to reproduce it, and the version (or commit). You
should get a first answer within a week. Once a fix is released the advisory is
published with credit to you, unless you prefer otherwise.

## Supported versions

Only the latest release receives security fixes.

## Security model

Know these before you deploy:

- **Repository access.** People open only the repositories they are members
  of, as *read* (browse, clone, see pipelines and job logs), *write* (also push
  and run pipelines) or *maintain* (also settings, CI/CD variables and members).
  Administrators open every repository and alone create and delete repositories
  and manage users and runners. Branches are not push-protected: anyone with
  write access can push to a protected branch, so its pipelines receive the
  protected variables. Give write access only to people you would trust with
  those secrets.
- **Runners execute code from the repository.** Any job in `.gitlab-ci.yml`
  runs on whichever runner picks it up, with that runner's access. A runner
  registered with `--privileged` (needed for `docker:dind` services) is
  root-equivalent on its host. Run runners on machines dedicated to CI, and
  never share one between teams that do not trust each other: runners serve
  every repository, so a job from one repository can reach what jobs from
  another leave on the same machine.
- **CI/CD variables** are encrypted at rest with the server's secret key, and
  decrypted when a job starts. Members with *maintain* access and administrators
  can read their values in the UI; each read is logged (`variable value shown`,
  with user and key). Protected variables reach only pipelines on
  protected branches and tags. Masking hides exact values in job logs; it is not
  a guarantee (a job can always print a secret transformed).
- **Run variables** (given when a pipeline is started, in *Run pipeline* or the
  API) are stored in plain text with the pipeline. The pipeline page shows only
  their keys. With *Show variables in the pipeline list* on, the list shows their
  values to everyone who can read the repository, except values marked *secret*,
  which are also masked in job logs. Pass credentials as CI/CD variables instead.
- **Tokens** (personal access tokens, runner tokens, job tokens) are stored as
  SHA-256 hashes. Job tokens are valid only while their job runs and only for
  cloning their own repository.
- **Pull mirrors** copy a repository from another server (GitHub, Bitbucket, …)
  and refuse pushes, so the upstream decides what runs: anyone who can push to
  the upstream's protected branch reaches the protected variables here. Protect
  that branch on the upstream. Only administrators set a mirror's URL, and only
  `https://` and SSH URLs are accepted (no local paths, `file://`, `ext::` or
  plain http; git runs with `GIT_ALLOW_PROTOCOL=https:ssh`). The mirror's SSH key
  is generated per repository, meant to be read-only on the upstream, and stored
  encrypted like CI/CD variables, as are its HTTPS and commit status tokens and
  webhook secret. The upstream's SSH host key is pinned at the first contact;
  check its fingerprint in *Settings → Mirror*. Webhook calls must be signed with
  the mirror's secret (HMAC-SHA256, compared in constant time); their content is
  never used, they only trigger a fetch.
- **Put TLS in front.** The server speaks plain HTTP; run it behind a reverse
  proxy with HTTPS (see `contrib/nginx.conf.example`) and set
  `RUST_AFARIANO_PUBLIC_URL` to the `https://` address so cookies are marked
  `Secure`.
