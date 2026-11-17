//! Two-step sign-in and its settings (see `rcommerce_core::services::two_factor`).
//!
//! - After a correct password, an account with a second factor gets a one-time
//!   `challenge` (403 `second_factor_required`) to answer with an authenticator code,
//!   a recovery code or one of its passkeys. Staff who must have a second factor and
//!   have none get a `setup_token` (403 `second_factor_enrolment_required`) that only
//!   opens the settings below; adding a factor then signs them in.
//! - A passkey also signs in on its own (`/auth/passkey/*`).
//! - Adding or removing a factor needs a recent proof of identity (sign-in or
//!   `/auth/reauth*` in the last 5 minutes): 403 `reauth_required` otherwise.

// Handlers answer with a ready response either way (a sign-in step is a 403 with its
// own body), as `auth.rs` does: the large `Err` is the response itself.
#![allow(clippy::result_large_err)]

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::IpAddr;
use uuid::Uuid;

use crate::middleware::{ClientIp, JwtAuth};
use crate::routes::auth::{login_throttled, sign_in};
use crate::state::AppState;
use rcommerce_core::config::StaffMfaPolicy;
use rcommerce_core::models::Customer;
use rcommerce_core::services::auth_session::{self, RevokeReason};
use rcommerce_core::services::two_factor::{
    self,
    challenge::{self, ChallengeKind},
    events::{self, Event},
    lockout::{self, Attempt},
    passkeys, recovery, totp, Factor, Factors, Removal, FRESH_SECS,
};
use rcommerce_core::services::{AuthService, JwtClaims, SignedIn, TokenType};
use rcommerce_core::Error;

type Reply = Result<Response, Response>;

/// A step of the sign-in, as the e-mail verification step answers:
/// `{"error": {"code": ..., "message": ..., <extra>}}`
fn step(status: StatusCode, code: &str, message: &str, extra: Value) -> Response {
    let mut error = json!({ "code": code, "message": message });
    if let (Some(fields), Value::Object(more)) = (error.as_object_mut(), extra) {
        fields.extend(more);
    }
    (status, Json(json!({ "error": error }))).into_response()
}

fn fail(e: Error) -> Response {
    e.into_response()
}

fn ip_of(client_ip: Option<Extension<ClientIp>>) -> IpAddr {
    client_ip
        .map(|ext| ext.0.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
}

async fn customer(state: &AppState, id: Uuid) -> Result<Customer, Response> {
    state
        .customer_service
        .find_by_id(id)
        .await
        .map_err(fail)?
        .ok_or_else(|| fail(Error::unauthorized("Account not found")))
}

/// Whether this account must have a second factor (`[security.mfa] staff = "required"`)
pub(crate) async fn staff_required(state: &AppState, customer: &Customer) -> Result<bool, Error> {
    let policy = state.auth_service.config().security.mfa.staff;
    if policy != StaffMfaPolicy::Required {
        return Ok(false);
    }
    two_factor::is_staff(state.db.pool(), customer.id, &customer.role).await
}

/// The second steps this account can use now
fn methods(state: &AppState, factors: &Factors) -> Vec<&'static str> {
    let mut methods = Vec::new();
    if factors.totp && state.totp_key.is_some() {
        methods.push("totp");
    }
    if factors.passkeys > 0 && state.passkeys.is_some() {
        methods.push("passkey");
    }
    if factors.recovery_codes_left > 0 {
        methods.push("recovery_code");
    }
    methods
}

/// The ways to add a second factor on this store
fn enrolment_methods(state: &AppState) -> Vec<&'static str> {
    let mut methods = Vec::new();
    if state.passkeys.is_some() {
        methods.push("passkey");
    }
    if state.totp_key.is_some() {
        methods.push("totp");
    }
    methods
}

/// After a correct password (and a verified e-mail): tokens, the second step, or
/// (staff without a second factor) the setup token
pub(crate) async fn after_password(
    state: &AppState,
    customer: Customer,
    remember_me: bool,
    ip: IpAddr,
) -> Reply {
    let pool = state.db.pool();
    let factors = two_factor::factors(pool, customer.id).await.map_err(fail)?;
    if factors.any() {
        let data = json!({ "remember_me": remember_me });
        let kind = ChallengeKind::SecondFactor;
        let id = challenge::create(pool, kind, Some(customer.id), data)
            .await
            .map_err(fail)?;
        let extra = json!({ "challenge": id, "methods": methods(state, &factors) });
        return Err(step(
            StatusCode::FORBIDDEN,
            "second_factor_required",
            "Confirm your sign-in with your second factor",
            extra,
        ));
    }
    // No second step: the right password ends the sign-in, so its failures are cleared
    state.login_throttle.clear_pair(ip, &customer.email);
    if staff_required(state, &customer).await.map_err(fail)? {
        let token = state
            .auth_service
            .generate_mfa_setup_token(customer.id, &customer.email, remember_me)
            .map_err(fail)?;
        let extra = json!({ "setup_token": token, "methods": enrolment_methods(state) });
        return Err(step(
            StatusCode::FORBIDDEN,
            "second_factor_enrolment_required",
            "Staff accounts need a second factor: add one to continue",
            extra,
        ));
    }
    finish(state, &customer, remember_me, &["pwd"], false, ip).await
}

/// Sign in and record it
async fn finish(
    state: &AppState,
    customer: &Customer,
    remember_me: bool,
    methods: &[&str],
    mfa: bool,
    ip: IpAddr,
) -> Reply {
    let tokens = sign_in(state, customer, remember_me, &SignedIn::now(methods, mfa))
        .await
        .map_err(fail)?;
    let detail = json!({ "method": methods.join("+") });
    let event = Event::own(customer.id, events::SIGN_IN, Some(ip), detail);
    events::record_quietly(state.db.pool(), event).await;
    Ok(Json(tokens).into_response())
}

/// Second step after a password
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecondFactorRequest {
    pub challenge: Uuid,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub recovery_code: Option<String>,
}

/// The live second-step challenge and its account, under the login throttle. The
/// attempt is counted on the challenge before the answer is checked.
async fn pending(state: &AppState, id: Uuid, ip: IpAddr) -> Result<(Customer, bool), Response> {
    let pool = state.db.pool();
    let found = challenge::reserve(pool, id, ChallengeKind::SecondFactor)
        .await
        .map_err(fail)?
        .and_then(|c| Some((c.customer_id?, c.data["remember_me"].as_bool())))
        .ok_or_else(|| fail(Error::unauthorized("Sign-in expired; start again")))?;
    let customer = customer(state, found.0).await?;
    if let Err(retry_after) = state.login_throttle.check_attempt(ip, &customer.email) {
        return Err(login_throttled(retry_after));
    }
    Ok((customer, found.1.unwrap_or(false)))
}

/// Record a wrong second step and answer 401
async fn wrong_step(state: &AppState, customer: &Customer, ip: IpAddr, flow: &str) -> Response {
    let detail = json!({ "flow": flow });
    let event = Event::own(customer.id, events::SECOND_FACTOR_FAILED, Some(ip), detail);
    events::record_quietly(state.db.pool(), event).await;
    fail(Error::unauthorized("Wrong code"))
}

/// Count a code attempt for the account, whatever the address; refused while its
/// codes are locked after too many wrong ones
async fn code_attempt(state: &AppState, customer: &Customer, ip: IpAddr) -> Result<(), Response> {
    let pool = state.db.pool();
    match lockout::reserve(pool, customer.id).await.map_err(fail)? {
        Attempt::Allowed => Ok(()),
        Attempt::Locked(seconds) => {
            let detail = json!({ "seconds": seconds });
            let event = Event::own(customer.id, events::SECOND_FACTOR_LOCKED, Some(ip), detail);
            events::record_quietly(pool, event).await;
            Err(login_throttled(seconds.max(1) as u64))
        }
    }
}

/// Check an authenticator or recovery code, counted for the account first; Some(amr
/// method) when right
async fn check_code(
    state: &AppState,
    customer: &Customer,
    ip: IpAddr,
    code: Option<&str>,
    recovery_code: Option<&str>,
) -> Result<Option<&'static str>, Response> {
    code_attempt(state, customer, ip).await?;
    let method = check_once(state, customer, code, recovery_code).await?;
    if method.is_some() {
        lockout::succeeded(state.db.pool(), customer.id)
            .await
            .map_err(fail)?;
    }
    Ok(method)
}

async fn check_once(
    state: &AppState,
    customer: &Customer,
    code: Option<&str>,
    recovery_code: Option<&str>,
) -> Result<Option<&'static str>, Response> {
    let pool = state.db.pool();
    if let Some(code) = code {
        let key = state
            .totp_key
            .as_ref()
            .ok_or_else(|| fail(Error::validation("Authenticator apps are not enabled")))?;
        let right = totp::check(pool, key, customer.id, code, false)
            .await
            .map_err(fail)?;
        return Ok(right.then_some("otp"));
    }
    if let Some(code) = recovery_code {
        let right = recovery::consume(pool, customer.id, code)
            .await
            .map_err(fail)?;
        return Ok(right.then_some("recovery"));
    }
    Err(fail(Error::validation("Give a code or a recovery code")))
}

/// `POST /auth/login/second-factor`: a code or a recovery code after the password
pub async fn second_factor(
    State(state): State<AppState>,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<SecondFactorRequest>,
) -> Reply {
    let ip = ip_of(client_ip);
    let (customer, remember_me) = pending(&state, body.challenge, ip).await?;
    let code = body.code.as_deref();
    let recovery = body.recovery_code.as_deref();
    let Some(method) = check_code(&state, &customer, ip, code, recovery).await? else {
        return Err(wrong_step(&state, &customer, ip, "sign_in").await);
    };
    let pool = state.db.pool();
    challenge::take(pool, body.challenge, ChallengeKind::SecondFactor)
        .await
        .map_err(fail)?
        .ok_or_else(|| fail(Error::unauthorized("Sign-in expired; start again")))?;
    state.login_throttle.clear_pair(ip, &customer.email);
    if method == "recovery" {
        let event = Event::own(customer.id, events::RECOVERY_CODE_USED, Some(ip), json!({}));
        events::record_quietly(pool, event).await;
    }
    finish(&state, &customer, remember_me, &["pwd", method], true, ip).await
}

fn passkeys_of(state: &AppState) -> Result<&passkeys::Passkeys, Response> {
    state
        .passkeys
        .as_ref()
        .ok_or_else(|| fail(Error::validation("Passkeys are not enabled on this store")))
}

/// The challenge of a sign-in waiting for its second step
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeRef {
    pub challenge: Uuid,
}

/// `POST /auth/login/second-factor/passkey/options`
pub async fn second_factor_passkey_options(
    State(state): State<AppState>,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<ChallengeRef>,
) -> Reply {
    let (customer, _) = pending(&state, body.challenge, ip_of(client_ip)).await?;
    let ceremony = passkeys_of(&state)?
        .start_for(
            state.db.pool(),
            customer.id,
            ChallengeKind::PasskeySecondFactor,
        )
        .await
        .map_err(fail)?;
    Ok(Json(ceremony).into_response())
}

/// A passkey's answer to a ceremony
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasskeyAnswer {
    #[serde(default)]
    pub challenge: Option<Uuid>,
    pub state_id: Uuid,
    pub credential: Value,
    #[serde(default)]
    pub remember_me: bool,
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST /auth/login/second-factor/passkey/verify`
pub async fn second_factor_passkey_verify(
    State(state): State<AppState>,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<PasskeyAnswer>,
) -> Reply {
    let ip = ip_of(client_ip);
    let id = body
        .challenge
        .ok_or_else(|| fail(Error::validation("challenge is required")))?;
    let (customer, remember_me) = pending(&state, id, ip).await?;
    let pool = state.db.pool();
    let kind = ChallengeKind::PasskeySecondFactor;
    let verified = passkeys_of(&state)?
        .finish_for(pool, customer.id, kind, body.state_id, &body.credential)
        .await;
    if verified.is_err() {
        return Err(wrong_step(&state, &customer, ip, "sign_in").await);
    }
    challenge::take(pool, id, ChallengeKind::SecondFactor)
        .await
        .map_err(fail)?
        .ok_or_else(|| fail(Error::unauthorized("Sign-in expired; start again")))?;
    state.login_throttle.clear_pair(ip, &customer.email);
    finish(&state, &customer, remember_me, &["pwd", "hwk"], true, ip).await
}

/// `POST /auth/passkey/options`: sign in with a passkey, no e-mail typed. Every
/// sign-in page asks for these, so they are left to the per-address rate limiter
/// (`[rate_limiting]`) rather than the login budgets, which page views would use up.
pub async fn passkey_sign_in_options(State(state): State<AppState>) -> Reply {
    let ceremony = passkeys_of(&state)?
        .start_sign_in(state.db.pool())
        .await
        .map_err(fail)?;
    Ok(Json(ceremony).into_response())
}

/// `POST /auth/passkey/verify`. An unknown passkey answers 404, so the page can tell
/// the password manager to forget it.
pub async fn passkey_sign_in_verify(
    State(state): State<AppState>,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<PasskeyAnswer>,
) -> Reply {
    let ip = ip_of(client_ip);
    if let Err(retry_after) = state.login_throttle.check_attempt(ip, "passkey") {
        return Err(login_throttled(retry_after));
    }
    let customer_id = passkeys_of(&state)?
        .finish_sign_in(state.db.pool(), body.state_id, &body.credential)
        .await
        .map_err(fail)?;
    let customer = customer(&state, customer_id).await?;
    state.login_throttle.clear_pair(ip, "passkey");
    finish(
        &state,
        &customer,
        body.remember_me,
        &["hwk", "user"],
        true,
        ip,
    )
    .await
}

/// The bearer token of a settings or re-verification request: an access token, or
/// (settings only) a setup token
fn bearer(
    state: &AppState,
    headers: &HeaderMap,
    setup_allowed: bool,
) -> Result<JwtClaims, Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(AuthService::extract_bearer_token)
        .ok_or_else(|| fail(Error::unauthorized("Sign in first")))?;
    let claims = state
        .auth_service
        .verify_access_or_setup_token(token)
        .map_err(fail)?;
    if claims.token_type == TokenType::MfaSetup && !setup_allowed {
        return Err(fail(Error::unauthorized("Invalid token type")));
    }
    Ok(claims)
}

/// Refuse a sensitive change without a recent proof of identity. A setup token is
/// one: it is given right after the password and lasts 15 minutes.
fn fresh(claims: &JwtClaims) -> Result<(), Response> {
    if claims.token_type == TokenType::MfaSetup || claims.is_fresh(FRESH_SECS) {
        return Ok(());
    }
    Err(step(
        StatusCode::FORBIDDEN,
        "reauth_required",
        "Confirm it is you to change this",
        json!({}),
    ))
}

/// `GET /customers/me/security`: the account's second factors
pub async fn security(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let claims = bearer(&state, &headers, true)?;
    let pool = state.db.pool();
    let customer = customer(&state, claims.sub).await?;
    let (factors, keys) = tokio::try_join!(
        two_factor::factors(pool, customer.id),
        passkeys::list(pool, customer.id)
    )
    .map_err(fail)?;
    let handle = passkeys::user_handle(pool, customer.id)
        .await
        .map_err(fail)?;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    Ok(Json(json!({
        "totp": factors.totp,
        "passkeys": keys,
        "recovery_codes_left": factors.recovery_codes_left,
        "second_factor_required": staff_required(&state, &customer).await.map_err(fail)?,
        "available": enrolment_methods(&state),
        "rp_id": state.passkeys.as_ref().map(|p| p.rp_id().to_string()),
        "webauthn_user_id": URL_SAFE_NO_PAD.encode(handle.as_bytes()),
        "recently_verified": claims.is_fresh(FRESH_SECS),
    }))
    .into_response())
}

/// A setup token adds the first factor only: once the account has one, signing in
/// asks for it
fn setup_still_open(claims: &JwtClaims, factors: &Factors) -> Result<(), Response> {
    if claims.token_type == TokenType::MfaSetup && factors.any() {
        return Err(fail(Error::unauthorized(
            "A second factor is set up: sign in with it",
        )));
    }
    Ok(())
}

/// After a first second factor: recovery codes (shown once); the sessions opened
/// with the password alone end (someone else may know it), and the caller gets a new
/// session with the second factor (for a setup token, the sign-in it was waiting for)
async fn after_adding(
    state: &AppState,
    claims: &JwtClaims,
    customer: &Customer,
    had_factor: bool,
    method: &str,
    ip: IpAddr,
) -> Result<Value, Response> {
    let pool = state.db.pool();
    let mut reply = json!({});
    if had_factor {
        return Ok(reply);
    }
    let codes = recovery::replace(pool, customer.id).await.map_err(fail)?;
    reply["recovery_codes"] = json!(codes);
    let reason = RevokeReason::SecondFactorAdded;
    auth_session::revoke_without_mfa(pool, customer.id, reason)
        .await
        .map_err(fail)?;
    let remember_me = claims.permissions.iter().any(|p| p == "remember_me");
    let how = SignedIn::now(&["pwd", method], true);
    let tokens = sign_in(state, customer, remember_me, &how)
        .await
        .map_err(fail)?;
    reply["session"] = json!(tokens);
    if claims.token_type == TokenType::MfaSetup {
        let detail = json!({ "method": format!("pwd+{method}") });
        let event = Event::own(customer.id, events::SIGN_IN, Some(ip), detail);
        events::record_quietly(pool, event).await;
    }
    Ok(reply)
}

/// `POST /customers/me/security/totp/setup`: a new secret for the authenticator app
pub async fn totp_setup(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let claims = bearer(&state, &headers, true)?;
    fresh(&claims)?;
    let has = two_factor::factors(state.db.pool(), claims.sub)
        .await
        .map_err(fail)?;
    setup_still_open(&claims, &has)?;
    let key = state
        .totp_key
        .as_ref()
        .ok_or_else(|| fail(Error::validation("Authenticator apps are not enabled")))?;
    let customer = customer(&state, claims.sub).await?;
    let security = &state.auth_service.config().security;
    let issuer = security
        .mfa
        .issuer
        .clone()
        .unwrap_or_else(|| security.webauthn.rp_name.clone());
    let enrolment = totp::begin(state.db.pool(), key, customer.id, &issuer, &customer.email)
        .await
        .map_err(fail)?;
    Ok(Json(enrolment).into_response())
}

/// An authenticator code
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeRequest {
    pub code: String,
}

/// `POST /customers/me/security/totp/confirm`: the app's first code turns it on
pub async fn totp_confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<CodeRequest>,
) -> Reply {
    let claims = bearer(&state, &headers, true)?;
    let key = state
        .totp_key
        .as_ref()
        .ok_or_else(|| fail(Error::validation("Authenticator apps are not enabled")))?;
    let customer = customer(&state, claims.sub).await?;
    let pool = state.db.pool();
    let had = two_factor::factors(pool, customer.id).await.map_err(fail)?;
    setup_still_open(&claims, &had)?;
    let ip = ip_of(client_ip);
    code_attempt(&state, &customer, ip).await?;
    let right = totp::check(pool, key, customer.id, &body.code, true)
        .await
        .map_err(fail)?;
    if !right {
        let detail = json!({ "flow": "setup" });
        let event = Event::own(customer.id, events::SECOND_FACTOR_FAILED, Some(ip), detail);
        events::record_quietly(pool, event).await;
        return Err(fail(Error::validation(
            "Wrong code: check the time on the phone",
        )));
    }
    lockout::succeeded(pool, customer.id).await.map_err(fail)?;
    let event = Event::own(customer.id, events::TOTP_ADDED, Some(ip), json!({}));
    events::record_quietly(pool, event).await;
    let reply = after_adding(&state, &claims, &customer, had.any(), "otp", ip).await?;
    Ok(Json(reply).into_response())
}

/// Remove a factor; staff who must have a second factor keep at least one
async fn remove_factor(state: &AppState, customer: &Customer, factor: &Factor) -> Reply {
    let keep_one = staff_required(state, customer).await.map_err(fail)?;
    let pool = state.db.pool();
    match two_factor::remove(pool, customer.id, factor, keep_one)
        .await
        .map_err(fail)?
    {
        Removal::Removed => Ok(StatusCode::NO_CONTENT.into_response()),
        Removal::NotFound => Err(fail(Error::not_found("No such second factor"))),
        Removal::LastOne => Err(step(
            StatusCode::CONFLICT,
            "second_factor_required_for_staff",
            "Staff accounts must keep a second factor: add another one first",
            json!({}),
        )),
    }
}

/// `DELETE /customers/me/security/totp`
pub async fn totp_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    client_ip: Option<Extension<ClientIp>>,
) -> Reply {
    let claims = bearer(&state, &headers, false)?;
    fresh(&claims)?;
    let customer = customer(&state, claims.sub).await?;
    let removed = remove_factor(&state, &customer, &Factor::Totp).await?;
    let ip = Some(ip_of(client_ip));
    let event = Event::own(customer.id, events::TOTP_REMOVED, ip, json!({}));
    events::record_quietly(state.db.pool(), event).await;
    Ok(removed)
}

/// `POST /customers/me/security/passkeys/options`: add a passkey
pub async fn passkey_add_options(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let claims = bearer(&state, &headers, true)?;
    fresh(&claims)?;
    let has = two_factor::factors(state.db.pool(), claims.sub)
        .await
        .map_err(fail)?;
    setup_still_open(&claims, &has)?;
    let customer = customer(&state, claims.sub).await?;
    let display = format!("{} {}", customer.first_name, customer.last_name);
    let ceremony = passkeys_of(&state)?
        .start_registration(
            state.db.pool(),
            customer.id,
            &customer.email,
            display.trim(),
        )
        .await
        .map_err(fail)?;
    Ok(Json(ceremony).into_response())
}

/// `POST /customers/me/security/passkeys/verify`
pub async fn passkey_add_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<PasskeyAnswer>,
) -> Reply {
    let claims = bearer(&state, &headers, true)?;
    let customer = customer(&state, claims.sub).await?;
    let pool = state.db.pool();
    let had = two_factor::factors(pool, customer.id).await.map_err(fail)?;
    setup_still_open(&claims, &had)?;
    let name: String = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Passkey")
        .chars()
        .take(100)
        .collect();
    let added = passkeys_of(&state)?
        .finish_registration(pool, customer.id, body.state_id, &body.credential, &name)
        .await
        .map_err(fail)?;
    let ip = ip_of(client_ip);
    let detail = json!({ "credential_id": added.id });
    let event = Event::own(customer.id, events::PASSKEY_ADDED, Some(ip), detail);
    events::record_quietly(pool, event).await;
    let mut reply = after_adding(&state, &claims, &customer, had.any(), "hwk", ip).await?;
    reply["passkey"] = json!(added);
    Ok(Json(reply).into_response())
}

/// A passkey's new name
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameRequest {
    pub name: String,
}

/// `PATCH /customers/me/security/passkeys/:id`: a label only, no proof needed
pub async fn passkey_rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RenameRequest>,
) -> Reply {
    let claims = bearer(&state, &headers, false)?;
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(fail(Error::validation("Name: 1 to 100 characters")));
    }
    let renamed = passkeys::rename(state.db.pool(), claims.sub, &id, name)
        .await
        .map_err(fail)?;
    if !renamed {
        return Err(fail(Error::not_found("Unknown passkey")));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /customers/me/security/passkeys/:id`
pub async fn passkey_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    client_ip: Option<Extension<ClientIp>>,
    Path(id): Path<String>,
) -> Reply {
    let claims = bearer(&state, &headers, false)?;
    fresh(&claims)?;
    let customer = customer(&state, claims.sub).await?;
    let pool = state.db.pool();
    remove_factor(&state, &customer, &Factor::Passkey(id.clone())).await?;
    let detail = json!({ "credential_id": id });
    let event = Event::own(
        customer.id,
        events::PASSKEY_REMOVED,
        Some(ip_of(client_ip)),
        detail,
    );
    events::record_quietly(pool, event).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /customers/me/security/recovery-codes`: new codes; the old ones stop working
pub async fn recovery_codes_renew(
    State(state): State<AppState>,
    headers: HeaderMap,
    client_ip: Option<Extension<ClientIp>>,
) -> Reply {
    let claims = bearer(&state, &headers, false)?;
    fresh(&claims)?;
    let pool = state.db.pool();
    let factors = two_factor::factors(pool, claims.sub).await.map_err(fail)?;
    if !factors.any() {
        return Err(fail(Error::validation("Add a second factor first")));
    }
    let codes = recovery::replace(pool, claims.sub).await.map_err(fail)?;
    let event = Event::own(
        claims.sub,
        events::RECOVERY_CODES_RENEWED,
        Some(ip_of(client_ip)),
        json!({}),
    );
    events::record_quietly(pool, event).await;
    Ok(Json(json!({ "recovery_codes": codes })).into_response())
}

/// Proof of identity for a sensitive change: the password when the account has no
/// second factor, else a code or a recovery code
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReauthRequest {
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub recovery_code: Option<String>,
}

/// A new access token for the proof just given: `methods` (a second factor makes it
/// `mfa`; a password alone does not, whatever the old token said)
fn reverified(state: &AppState, customer: &Customer, methods: &[&str], mfa: bool) -> Reply {
    let how = SignedIn::now(methods, mfa);
    let token = state
        .auth_service
        .issue_access_token(customer.id, &customer.email, &customer.role, &how)
        .map_err(fail)?;
    Ok(Json(json!({
        "access_token": token,
        "token_type": "Bearer",
        "expires_in": state.auth_service.access_token_lifetime_secs(),
    }))
    .into_response())
}

/// Check the password of the signed-in account
fn right_password(state: &AppState, customer: &Customer, password: &str) -> Result<bool, Response> {
    let Some(hash) = customer.password_hash.as_deref() else {
        return Ok(false);
    };
    if password.is_empty() || password.len() > 128 {
        return Ok(false);
    }
    Ok(state
        .auth_service
        .verify_password(password, hash)
        .map_err(fail)?
        .0)
}

/// `POST /auth/reauth`: confirm it is you before a sensitive change
pub async fn reauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<ReauthRequest>,
) -> Reply {
    let claims = bearer(&state, &headers, false)?;
    let ip = ip_of(client_ip);
    let customer = customer(&state, claims.sub).await?;
    if let Err(retry_after) = state.login_throttle.check_attempt(ip, &customer.email) {
        return Err(login_throttled(retry_after));
    }
    let pool = state.db.pool();
    let factors = two_factor::factors(pool, customer.id).await.map_err(fail)?;
    let method = if factors.any() {
        let (code, recovery) = (body.code.as_deref(), body.recovery_code.as_deref());
        match check_code(&state, &customer, ip, code, recovery).await? {
            Some(method) => method,
            None => return Err(wrong_step(&state, &customer, ip, "reauth").await),
        }
    } else if right_password(&state, &customer, body.password.as_deref().unwrap_or(""))? {
        "pwd"
    } else {
        let detail = json!({ "flow": "reauth", "method": "pwd" });
        let event = Event::own(customer.id, events::SECOND_FACTOR_FAILED, Some(ip), detail);
        events::record_quietly(pool, event).await;
        return Err(fail(Error::unauthorized("Wrong password")));
    };
    state.login_throttle.clear_pair(ip, &customer.email);
    if method == "recovery" {
        let event = Event::own(customer.id, events::RECOVERY_CODE_USED, Some(ip), json!({}));
        events::record_quietly(pool, event).await;
    }
    let event = Event::own(customer.id, events::REAUTHENTICATED, Some(ip), json!({}));
    events::record_quietly(pool, event).await;
    reverified(&state, &customer, &[method], method != "pwd")
}

/// `POST /auth/reauth/passkey/options`: confirm it is you with one of your passkeys
pub async fn reauth_passkey_options(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let claims = bearer(&state, &headers, false)?;
    let ceremony = passkeys_of(&state)?
        .start_for(state.db.pool(), claims.sub, ChallengeKind::PasskeyReauth)
        .await
        .map_err(fail)?;
    Ok(Json(ceremony).into_response())
}

/// `POST /auth/reauth/passkey/verify`
pub async fn reauth_passkey_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    client_ip: Option<Extension<ClientIp>>,
    Json(body): Json<PasskeyAnswer>,
) -> Reply {
    let claims = bearer(&state, &headers, false)?;
    let customer = customer(&state, claims.sub).await?;
    let pool = state.db.pool();
    let ip = ip_of(client_ip);
    let kind = ChallengeKind::PasskeyReauth;
    let verified = passkeys_of(&state)?
        .finish_for(pool, customer.id, kind, body.state_id, &body.credential)
        .await;
    if let Err(e) = verified {
        let detail = json!({ "flow": "reauth", "method": "hwk" });
        let event = Event::own(customer.id, events::SECOND_FACTOR_FAILED, Some(ip), detail);
        events::record_quietly(pool, event).await;
        return Err(fail(e));
    }
    let event = Event::own(customer.id, events::REAUTHENTICATED, Some(ip), json!({}));
    events::record_quietly(pool, event).await;
    reverified(&state, &customer, &["hwk"], true)
}

/// `POST /admin/staff/:id/mfa/reset`: an admin removes every second factor of an
/// account (a lost phone) and ends its sessions. Needs a recent proof of identity.
pub async fn admin_reset(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    client_ip: Option<Extension<ClientIp>>,
    Path(id): Path<Uuid>,
) -> Reply {
    if !auth.is_admin() {
        return Err(fail(Error::forbidden("Only admins reset second factors")));
    }
    if !auth.is_fresh(FRESH_SECS) {
        let message = "Confirm it is you to change this";
        return Err(step(
            StatusCode::FORBIDDEN,
            "reauth_required",
            message,
            json!({}),
        ));
    }
    let pool = state.db.pool();
    let removed = two_factor::reset(pool, id).await.map_err(fail)?;
    auth_session::revoke_all(pool, id, RevokeReason::FactorsReset)
        .await
        .map_err(fail)?;
    let event = Event {
        customer_id: id,
        actor_id: Some(auth.customer_id),
        kind: events::FACTORS_RESET,
        ip: Some(ip_of(client_ip)),
        detail: json!({ "removed": removed }),
    };
    events::record_quietly(pool, event).await;
    Ok(Json(json!({ "removed": removed })).into_response())
}

/// `Cache-Control: no-store` on answers that carry tokens, codes or secrets
pub(crate) async fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

/// Sign-in steps, passkey sign-in, re-verification and the account's settings. The
/// handlers read the bearer token themselves (an access or a setup token).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/login/second-factor", post(second_factor))
        .route(
            "/auth/login/second-factor/passkey/options",
            post(second_factor_passkey_options),
        )
        .route(
            "/auth/login/second-factor/passkey/verify",
            post(second_factor_passkey_verify),
        )
        .route("/auth/passkey/options", post(passkey_sign_in_options))
        .route("/auth/passkey/verify", post(passkey_sign_in_verify))
        .route("/auth/reauth", post(reauth))
        .route("/auth/reauth/passkey/options", post(reauth_passkey_options))
        .route("/auth/reauth/passkey/verify", post(reauth_passkey_verify))
        .route("/customers/me/security", get(security))
        .route("/customers/me/security/totp/setup", post(totp_setup))
        .route("/customers/me/security/totp/confirm", post(totp_confirm))
        .route(
            "/customers/me/security/totp",
            axum::routing::delete(totp_remove),
        )
        .route(
            "/customers/me/security/passkeys/options",
            post(passkey_add_options),
        )
        .route(
            "/customers/me/security/passkeys/verify",
            post(passkey_add_verify),
        )
        .route(
            "/customers/me/security/passkeys/:id",
            patch(passkey_rename).delete(passkey_remove),
        )
        .route(
            "/customers/me/security/recovery-codes",
            post(recovery_codes_renew),
        )
        .layer(axum::middleware::map_response(no_store))
}

/// Admin only: reset an account's second factors
pub fn admin_router() -> Router<AppState> {
    Router::new().route("/admin/staff/:id/mfa/reset", post(admin_reset))
}
