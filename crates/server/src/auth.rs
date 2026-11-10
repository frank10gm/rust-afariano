//! Sign-in (local passwords, standard OpenID Connect, and "Rich" auth servers
//! such as auth.hartwellbridge.com), DB-backed sessions, first-admin setup,
//! and token lookups for git, the API and runners.

use askama::Template;
use axum::Form;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::prelude::*;
use chrono::{Duration, Utc};
use rust_afariano_proto as proto;
use serde::Deserialize;
use serde_json::Value;

use crate::accounts;
use crate::app::{AppState, Shared};
use crate::error::{AppError, AppResult};
use crate::models::{Job, RUNNER_COLS, Runner, User};
use crate::util::{b64url, ct_eq, hash_token, new_token, sha256};

const SESSION_COOKIE: &str = "rf_session";
const STATE_COOKIE: &str = "rf_login_state";
const VERIFIER_COOKIE: &str = "rf_login_verifier";
const NONCE_COOKIE: &str = "rf_login_nonce";
const NEXT_COOKIE: &str = "rf_login_next";
const LOGIN_TTL_SECS: i64 = 600;

pub fn cookies(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|p| p.trim().split_once('='))
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    cookies(headers).find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// SameSite=Lax: identity-provider callbacks are cross-site top-level
/// navigations, and Strict would withhold the state/verifier cookies.
fn set_cookie(state: &AppState, name: &str, value: &str, max_age: i64) -> HeaderValue {
    let secure = if state.cfg.secure_cookies() { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .expect("cookie header")
}

fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n.to_string(),
        _ => "/".to_string(),
    }
}

// ------------------------------------------------------------------ sign-in page

#[derive(Template)]
#[template(path = "signin.html")]
struct SignInPage {
    password_login: bool,
    oidc_name: String,
    rich_name: String,
    next: String,
    next_q: String,
    email: String,
    error: String,
    setup_open: bool,
}

fn sign_in_page(state: &AppState, next: &str, email: &str, error: &str, status: StatusCode) -> AppResult<Response> {
    let page = SignInPage {
        password_login: state.cfg.password_login,
        oidc_name: if state.cfg.oidc_enabled() { state.cfg.oidc_name.clone() } else { String::new() },
        rich_name: if state.cfg.rich_auth_url.is_some() { state.cfg.rich_auth_name.clone() } else { String::new() },
        next: next.to_string(),
        next_q: urlencode(next),
        email: email.to_string(),
        error: error.to_string(),
        setup_open: state.setup_token.lock().expect("setup token").is_some(),
    };
    let mut resp = (status, Html(page.render()?)).into_response();
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

#[derive(Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

/// The sign-in page; with a single external method and no passwords it goes
/// straight to the identity provider.
pub async fn login(State(state): State<Shared>, Query(q): Query<LoginQuery>) -> AppResult<Response> {
    let next = safe_next(q.next.as_deref());
    let cfg = &state.cfg;
    if !cfg.password_login {
        match (cfg.oidc_enabled(), cfg.rich_auth_url.is_some()) {
            (true, false) => return Ok(Redirect::to(&format!("/auth/oidc?next={}", urlencode(&next))).into_response()),
            (false, true) => return Ok(Redirect::to(&format!("/auth/rich?next={}", urlencode(&next))).into_response()),
            _ => {}
        }
    }
    sign_in_page(&state, &next, "", "", StatusCode::OK)
}

#[derive(Deserialize)]
pub struct PasswordForm {
    email: String,
    password: String,
    #[serde(default)]
    next: String,
}

pub async fn password_login(State(state): State<Shared>, Form(f): Form<PasswordForm>) -> AppResult<Response> {
    if !state.cfg.password_login {
        return Err(AppError::NotFound);
    }
    let next = safe_next(Some(&f.next));
    let email = f.email.trim().to_lowercase();
    if state.limiter.blocked(&email) {
        let msg = "Too many failed attempts for this account. Wait 15 minutes, or ask an administrator.";
        return sign_in_page(&state, &next, &email, msg, StatusCode::TOO_MANY_REQUESTS);
    }
    let account = accounts::find_by_email(&state.db, &email).await?;
    let (hash, usable) = match &account {
        Some(a) => (a.password_hash.clone(), !a.disabled && a.password_hash.is_some()),
        None => (None, false),
    };
    // Unknown emails still pay for one hash, so timing does not reveal accounts.
    let stored = hash.unwrap_or_else(|| dummy_hash().to_string());
    let ok = accounts::verify_password(&state, f.password, stored).await && usable;
    match account {
        Some(a) if ok => {
            state.limiter.clear(&email);
            start_session(&state, a.id, &next).await
        }
        _ => {
            state.limiter.fail(&email);
            tracing::warn!(email = %email, "failed password sign-in");
            sign_in_page(&state, &next, &email, "Wrong email or password.", StatusCode::UNAUTHORIZED)
        }
    }
}

fn dummy_hash() -> &'static str {
    // A valid Argon2id hash of a random password, same parameters as real ones.
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$0vF6m7oX3pQ3W8G4l2h1tq8F0cVq4lq8i4k0Fz6s7aE"
}

// ------------------------------------------------------------------ OpenID Connect

#[derive(Clone, Deserialize)]
pub struct OidcProvider {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

async fn oidc_provider(state: &AppState) -> AppResult<OidcProvider> {
    let mut cached = state.oidc.lock().await;
    if let Some(p) = cached.as_ref() {
        return Ok(p.clone());
    }
    let issuer = state.cfg.oidc_issuer.clone().ok_or(AppError::NotFound)?;
    let url = format!("{issuer}/.well-known/openid-configuration");
    let provider: OidcProvider = state
        .http
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| anyhow::anyhow!("OIDC discovery {url}: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("OIDC discovery {url}: {e}"))?;
    if provider.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
        return Err(
            anyhow::anyhow!("OIDC issuer mismatch: configured {issuer}, provider says {}", provider.issuer).into()
        );
    }
    *cached = Some(provider.clone());
    Ok(provider)
}

pub async fn oidc_start(State(state): State<Shared>, Query(q): Query<LoginQuery>) -> AppResult<Response> {
    let provider = oidc_provider(&state).await?;
    let next = safe_next(q.next.as_deref());
    let client_id = state.cfg.oidc_client_id.clone().ok_or(AppError::NotFound)?;
    let verifier = new_token("");
    let login_state = new_token("");
    let nonce = new_token("");
    let sep = if provider.authorization_endpoint.contains('?') { '&' } else { '?' };
    let url = format!(
        "{}{sep}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        provider.authorization_endpoint,
        urlencode(&client_id),
        urlencode(&format!("{}/auth/oidc/callback", state.cfg.public_url)),
        urlencode("openid email profile"),
        urlencode(&login_state),
        urlencode(&nonce),
        urlencode(&b64url(&sha256(verifier.as_bytes()))),
    );
    Ok(redirect_with_login_cookies(&state, &url, &verifier, &login_state, &nonce, &next))
}

fn redirect_with_login_cookies(
    state: &AppState,
    url: &str,
    verifier: &str,
    login_state: &str,
    nonce: &str,
    next: &str,
) -> Response {
    let mut resp = Redirect::to(url).into_response();
    let h = resp.headers_mut();
    h.append(header::SET_COOKIE, set_cookie(state, VERIFIER_COOKIE, verifier, LOGIN_TTL_SECS));
    h.append(header::SET_COOKIE, set_cookie(state, STATE_COOKIE, login_state, LOGIN_TTL_SECS));
    h.append(header::SET_COOKIE, set_cookie(state, NONCE_COOKIE, nonce, LOGIN_TTL_SECS));
    h.append(header::SET_COOKIE, set_cookie(state, NEXT_COOKIE, &urlencode(next), LOGIN_TTL_SECS));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

struct LoginCookies {
    verifier: String,
    nonce: String,
    next: String,
}

/// Checks `state` against the cookie set when the login started.
fn check_callback(headers: &HeaderMap, q: &CallbackQuery) -> Result<(String, LoginCookies), &'static str> {
    let expected = cookie_value(headers, STATE_COOKIE).unwrap_or("");
    let verifier = cookie_value(headers, VERIFIER_COOKIE).unwrap_or("").to_string();
    let nonce = cookie_value(headers, NONCE_COOKIE).unwrap_or("").to_string();
    let next = safe_next(cookie_value(headers, NEXT_COOKIE).map(urldecode).as_deref());
    if q.error.is_some() {
        return Err("Sign-in was cancelled or refused by the identity provider.");
    }
    let (Some(code), Some(returned)) = (q.code.clone(), q.state.as_deref()) else {
        return Err("The identity provider sent an incomplete answer. Please try again.");
    };
    if expected.is_empty() || verifier.is_empty() || !ct_eq(returned.as_bytes(), expected.as_bytes()) {
        return Err("This sign-in link expired or was opened in another browser. Please try again.");
    }
    Ok((code, LoginCookies { verifier, nonce, next }))
}

pub async fn oidc_callback(
    State(state): State<Shared>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (code, c) = match check_callback(&headers, &q) {
        Ok(v) => v,
        Err(msg) => return sign_in_page(&state, "/", "", msg, StatusCode::BAD_REQUEST),
    };
    let provider = oidc_provider(&state).await?;
    match oidc_identity(&state, &provider, &code, &c).await {
        Ok((email, name)) => finish_sso(&state, &email, &name, &c.next).await,
        Err(e) => {
            tracing::warn!("OIDC sign-in failed: {e:#}");
            let msg = format!("Sign-in with {} failed. Please try again.", state.cfg.oidc_name);
            sign_in_page(&state, &c.next, "", &msg, StatusCode::BAD_GATEWAY)
        }
    }
}

/// Code → tokens → (email, name). The ID token comes straight from the token
/// endpoint over TLS, which OIDC Core §3.1.3.7 accepts in place of a signature
/// check; issuer, audience, expiry and nonce are still verified.
async fn oidc_identity(
    state: &AppState,
    provider: &OidcProvider,
    code: &str,
    c: &LoginCookies,
) -> anyhow::Result<(String, String)> {
    let cfg = &state.cfg;
    let client_id = cfg.oidc_client_id.clone().unwrap_or_default();
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", format!("{}/auth/oidc/callback", cfg.public_url)),
        ("code_verifier", c.verifier.clone()),
        ("client_id", client_id.clone()),
    ];
    let mut req = state.http.post(&provider.token_endpoint).header(header::ACCEPT, "application/json");
    if let Some(secret) = &cfg.oidc_client_secret {
        let methods = &provider.token_endpoint_auth_methods_supported;
        let post_only =
            methods.iter().any(|m| m == "client_secret_post") && !methods.iter().any(|m| m == "client_secret_basic");
        if post_only {
            form.push(("client_secret", secret.clone()));
        } else {
            let basic = BASE64_STANDARD.encode(format!("{}:{}", urlencode(&client_id), urlencode(secret)));
            req = req.header(header::AUTHORIZATION, format!("Basic {basic}"));
        }
    }
    let body = form.iter().map(|(k, v)| format!("{k}={}", urlencode(v))).collect::<Vec<_>>().join("&");
    let resp = req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
        anyhow::bail!("token endpoint answered {status}: {text}");
    }
    let tokens: Value = resp.json().await?;
    let id_token = tokens["id_token"].as_str().ok_or_else(|| anyhow::anyhow!("no id_token in the token response"))?;
    let claims = jwt_claims(id_token)?;
    if claims["iss"].as_str().map(|s| s.trim_end_matches('/')) != Some(provider.issuer.trim_end_matches('/')) {
        anyhow::bail!("id_token issuer {:?} is not {}", claims["iss"], provider.issuer);
    }
    let aud_ok = match &claims["aud"] {
        Value::String(a) => *a == client_id,
        Value::Array(list) => list.iter().any(|a| a.as_str() == Some(&client_id)),
        _ => false,
    };
    if !aud_ok {
        anyhow::bail!("id_token audience does not include this client");
    }
    if claims["exp"].as_i64().is_none_or(|exp| exp + 60 < Utc::now().timestamp()) {
        anyhow::bail!("id_token expired");
    }
    if claims["nonce"].as_str() != Some(c.nonce.as_str()) {
        anyhow::bail!("id_token nonce mismatch");
    }
    let mut info = claims;
    if info["email"].as_str().is_none()
        && let (Some(endpoint), Some(access)) = (&provider.userinfo_endpoint, tokens["access_token"].as_str())
    {
        info = state.http.get(endpoint).bearer_auth(access).send().await?.error_for_status()?.json().await?;
    }
    let email =
        info["email"].as_str().ok_or_else(|| anyhow::anyhow!("the provider shared no email (scope `email`)"))?;
    let verified = match &info["email_verified"] {
        Value::Bool(b) => *b,
        Value::String(s) => s == "true",
        _ => true, // not stated: trust the provider
    };
    if !verified {
        anyhow::bail!("{email} is not verified at the identity provider");
    }
    let name = info["name"].as_str().or(info["preferred_username"].as_str()).unwrap_or("").to_string();
    Ok((email.to_string(), name))
}

fn jwt_claims(jwt: &str) -> anyhow::Result<Value> {
    let payload = jwt.split('.').nth(1).ok_or_else(|| anyhow::anyhow!("malformed id_token"))?;
    let bytes = BASE64_URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn finish_sso(state: &AppState, email: &str, name: &str, next: &str) -> AppResult<Response> {
    match accounts::sso_user(state, email, name).await? {
        Ok(user_id) => start_session(state, user_id, next).await,
        Err(msg) => {
            tracing::warn!(email = %email, "single sign-on refused: {msg}");
            sign_in_page(state, next, "", &msg, StatusCode::FORBIDDEN)
        }
    }
}

// ------------------------------------------------------------------ Rich auth server

/// Login on a "Rich" auth server (auth.hartwellbridge.com): PKCE with
/// `/api/request-login` and `/auth/token`, which answers with a `user` object.
pub async fn rich_start(State(state): State<Shared>, Query(q): Query<LoginQuery>) -> AppResult<Response> {
    let base = state.cfg.rich_auth_url.clone().ok_or(AppError::NotFound)?;
    let next = safe_next(q.next.as_deref());
    let verifier = new_token("");
    let login_state = new_token("");
    let url = format!(
        "{base}/api/request-login?redirect_uri={}&state={}&code_challenge={}&mode=login",
        urlencode(&format!("{}/auth/callback", state.cfg.public_url)),
        urlencode(&login_state),
        urlencode(&b64url(&sha256(verifier.as_bytes()))),
    );
    Ok(redirect_with_login_cookies(&state, &url, &verifier, &login_state, "", &next))
}

pub async fn rich_callback(
    State(state): State<Shared>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let base = state.cfg.rich_auth_url.clone().ok_or(AppError::NotFound)?;
    let (code, c) = match check_callback(&headers, &q) {
        Ok(v) => v,
        Err(msg) => return sign_in_page(&state, "/", "", msg, StatusCode::BAD_REQUEST),
    };
    let resp = state
        .http
        .post(format!("{base}/auth/token"))
        // The server reads the verifier from `code_verifier`; `ver` is for older builds.
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "code_verifier": c.verifier,
            "ver": c.verifier,
            "redirect_uri": format!("{}/auth/callback", state.cfg.public_url),
        }))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("auth server unreachable: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
        tracing::error!("token exchange failed: HTTP {status}: {body}");
        let msg = format!("Sign-in with {} failed. Please try again.", state.cfg.rich_auth_name);
        return sign_in_page(&state, &c.next, "", &msg, StatusCode::BAD_GATEWAY);
    }
    let tokens: Value = resp.json().await.map_err(|e| anyhow::anyhow!("token response: {e}"))?;
    let user = &tokens["user"];
    let email = user["email"].as_str().unwrap_or("").trim().to_lowercase();
    if email.is_empty() {
        return Err(anyhow::anyhow!("auth server returned no email").into());
    }
    let name = user["name"].as_str().or(user["username"].as_str()).unwrap_or("").to_string();
    finish_sso(&state, &email, &name, &c.next).await
}

// ------------------------------------------------------------------ first admin

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
    token: String,
    email: String,
    name: String,
    error: String,
}

#[derive(Deserialize)]
pub struct SetupQuery {
    #[serde(default)]
    token: String,
}

fn setup_token_ok(state: &AppState, token: &str) -> bool {
    let guard = state.setup_token.lock().expect("setup token");
    guard.as_deref().is_some_and(|t| ct_eq(t.as_bytes(), token.as_bytes()))
}

pub async fn setup_form(State(state): State<Shared>, Query(q): Query<SetupQuery>) -> AppResult<Response> {
    if !setup_token_ok(&state, &q.token) {
        return Ok(Redirect::to("/auth/login").into_response());
    }
    Ok(Html(SetupPage { token: q.token, email: String::new(), name: String::new(), error: String::new() }.render()?)
        .into_response())
}

#[derive(Deserialize)]
pub struct SetupForm {
    token: String,
    email: String,
    #[serde(default)]
    name: String,
    password: String,
    password2: String,
}

pub async fn setup_submit(State(state): State<Shared>, Form(f): Form<SetupForm>) -> AppResult<Response> {
    if !setup_token_ok(&state, &f.token) || accounts::user_count(&state.db).await? > 0 {
        return Ok(Redirect::to("/auth/login").into_response());
    }
    let error = if !accounts::valid_email(&f.email) {
        Some("Enter a valid email address.")
    } else {
        accounts::check_new_password(&f.password, &f.password2).err()
    };
    if let Some(error) = error {
        let page = SetupPage { token: f.token, email: f.email, name: f.name, error: error.into() };
        return Ok((StatusCode::BAD_REQUEST, Html(page.render()?)).into_response());
    }
    let hash = accounts::hash_password(&state, f.password).await?;
    let id = accounts::create_user(&state.db, &f.email, &f.name, Some(&hash), true).await?;
    state.close_setup();
    tracing::info!(email = %f.email.trim().to_lowercase(), "first admin created through the setup link");
    start_session(&state, id, "/").await
}

// ------------------------------------------------------------------ sessions

async fn start_session(state: &AppState, user_id: i64, next: &str) -> AppResult<Response> {
    sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1").bind(user_id).execute(&state.db).await?;
    let token = new_token("");
    let ttl = state.cfg.session_ttl_hours;
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(hash_token(&token))
        .bind(user_id)
        .bind(Utc::now() + Duration::hours(ttl))
        .execute(&state.db)
        .await?;
    let mut resp = Redirect::to(next).into_response();
    let h = resp.headers_mut();
    h.append(header::SET_COOKIE, set_cookie(state, SESSION_COOKIE, &token, ttl * 3600));
    for c in [STATE_COOKIE, VERIFIER_COOKIE, NONCE_COOKIE, NEXT_COOKIE] {
        h.append(header::SET_COOKIE, set_cookie(state, c, "", 0));
    }
    Ok(resp)
}

pub async fn logout(State(state): State<Shared>, headers: HeaderMap) -> AppResult<Response> {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1").bind(hash_token(token)).execute(&state.db).await?;
    }
    let mut resp = Redirect::to("/signed-out").into_response();
    resp.headers_mut().append(header::SET_COOKIE, set_cookie(&state, SESSION_COOKIE, "", 0));
    Ok(resp)
}

/// Ends every session of a user (password changed, account disabled).
pub async fn end_sessions(state: &AppState, user_id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1").bind(user_id).execute(&state.db).await.map(|_| ())
}

/// The signed-in user. Pages redirect to the sign-in page when there is none.
#[derive(Debug, Clone)]
pub struct CurrentUser(pub User);

impl FromRequestParts<Shared> for CurrentUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &Shared) -> Result<Self, Self::Rejection> {
        let user = match cookie_value(&parts.headers, SESSION_COOKIE) {
            Some(token) => sqlx::query_as::<_, User>(
                "SELECT u.id, u.email, u.name, u.is_admin FROM sessions s JOIN users u ON u.id = s.user_id \
                 WHERE s.token_hash = $1 AND s.expires_at > now() AND NOT u.disabled",
            )
            .bind(hash_token(token))
            .fetch_optional(&state.db)
            .await
            .map_err(|e| AppError::from(e).into_response())?,
            None => None,
        };
        match user {
            Some(u) => Ok(CurrentUser(u)),
            None if parts.method == Method::GET => {
                let next = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                Err(Redirect::to(&format!("/auth/login?next={}", urlencode(next))).into_response())
            }
            None => Err(AppError::Unauthorized.into_response()),
        }
    }
}

/// A signed-in administrator (users, runners).
pub struct AdminUser(pub User);

impl FromRequestParts<Shared> for AdminUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &Shared) -> Result<Self, Self::Rejection> {
        let CurrentUser(user) = CurrentUser::from_request_parts(parts, state).await?;
        if user.is_admin { Ok(AdminUser(user)) } else { Err(AppError::Forbidden.into_response()) }
    }
}

/// A user authenticated by `Authorization: Bearer rfp_…` (the JSON API).
pub struct TokenUser2(pub User);

impl FromRequestParts<Shared> for TokenUser2 {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &Shared) -> Result<Self, Self::Rejection> {
        let token = bearer(&parts.headers).ok_or(AppError::Unauthorized)?;
        user_by_pat(state, token).await?.map(TokenUser2).ok_or(AppError::Unauthorized)
    }
}

/// Caller of the pipeline calls that a job may make: an account (`rfp_…`), or a
/// running job with its own `CI_JOB_TOKEN` (`rfj_…`), which reaches only its own
/// repository (`access::job_repo`).
pub enum ApiCaller {
    User(User),
    Job(Box<Job>),
}

impl FromRequestParts<Shared> for ApiCaller {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &Shared) -> Result<Self, Self::Rejection> {
        let token = bearer(&parts.headers).ok_or(AppError::Unauthorized)?;
        if let Some(job) = job_by_token(state, token).await? {
            return Ok(Self::Job(Box::new(crate::pipelines::get_job(state, job.id).await?)));
        }
        user_by_pat(state, token).await?.map(Self::User).ok_or(AppError::Unauthorized)
    }
}

pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

pub async fn user_by_pat(state: &AppState, token: &str) -> sqlx::Result<Option<User>> {
    if !token.starts_with(proto::PAT_PREFIX) {
        return Ok(None);
    }
    sqlx::query_as::<_, User>(
        "UPDATE access_tokens t SET last_used_at = now() FROM users u \
         WHERE t.token_hash = $1 AND u.id = t.user_id AND NOT u.disabled \
         AND (t.expires_at IS NULL OR t.expires_at > now()) \
         RETURNING u.id, u.email, u.name, u.is_admin",
    )
    .bind(hash_token(token))
    .fetch_optional(&state.db)
    .await
}

pub struct JobAuth {
    pub id: i64,
    pub repo_id: i64,
    pub cancel_requested: bool,
}

pub async fn job_by_token(state: &AppState, token: &str) -> sqlx::Result<Option<JobAuth>> {
    if !token.starts_with(proto::JOB_TOKEN_PREFIX) {
        return Ok(None);
    }
    let row: Option<(i64, i64, bool)> =
        sqlx::query_as("SELECT id, repo_id, cancel_requested FROM jobs WHERE token_hash = $1 AND status = 'running'")
            .bind(hash_token(token))
            .fetch_optional(&state.db)
            .await?;
    Ok(row.map(|(id, repo_id, cancel_requested)| JobAuth { id, repo_id, cancel_requested }))
}

pub async fn runner_by_token(state: &AppState, token: &str) -> sqlx::Result<Option<Runner>> {
    if !token.starts_with(proto::RUNNER_TOKEN_PREFIX) {
        return Ok(None);
    }
    sqlx::query_as::<_, Runner>(sql!(
        "UPDATE runners SET contacted_at = now() WHERE token_hash = $1 RETURNING {RUNNER_COLS}"
    ))
    .bind(hash_token(token))
    .fetch_optional(&state.db)
    .await
}

/// Cookie-authenticated POSTs must come from our own pages (CSRF defence on
/// top of SameSite=Lax).
pub async fn same_origin(
    State(state): State<Shared>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .or_else(|| req.headers().get(header::REFERER))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let ours = &state.cfg.public_url;
        if !(origin == ours || origin.starts_with(&format!("{ours}/"))) {
            return AppError::Forbidden.into_response();
        }
    }
    next.run(req).await
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_is_local_only() {
        assert_eq!(safe_next(Some("/hw/x")), "/hw/x");
        assert_eq!(safe_next(Some("//evil.com")), "/");
        assert_eq!(safe_next(Some("https://evil.com")), "/");
        assert_eq!(safe_next(Some("/\\evil.com")), "/");
        assert_eq!(safe_next(None), "/");
    }

    #[test]
    fn url_round_trip() {
        let s = "/a b/ç?x=1&y=%";
        assert_eq!(urldecode(&urlencode(s)), s);
        assert_eq!(urldecode("%2"), "%2");
    }

    #[test]
    fn jwt_payload() {
        let payload = BASE64_URL_SAFE_NO_PAD.encode(br#"{"iss":"https://id.example.com","aud":"x","nonce":"n"}"#);
        let claims = jwt_claims(&format!("e30.{payload}.sig")).unwrap();
        assert_eq!(claims["iss"], "https://id.example.com");
        assert!(jwt_claims("nodots").is_err());
    }

    #[test]
    fn dummy_hash_parses() {
        assert!(argon2::password_hash::phc::PasswordHash::new(dummy_hash()).is_ok());
    }
}
