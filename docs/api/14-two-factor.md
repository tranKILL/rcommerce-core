# Two-step sign-in: passkeys, authenticator apps, recovery codes

A second factor protects an account when its password leaks. rCommerce offers:

- **Passkeys** (WebAuthn): the browser or phone signs with a key that never leaves the
  device, after a fingerprint, face or PIN (user verification is required). A passkey
  signs in on its own and counts as two factors; it cannot be phished.
- **Authenticator apps** (TOTP, RFC 6238): 6 digits every 30 seconds, SHA-1. A code is
  accepted in its own step and the next (for a late clock or person), never before, and
  works once.
- **Recovery codes**: 10 single-use codes, shown once when the first factor is added.

**Guessing:** every code or recovery code attempt counts, before it is checked, against
the challenge (5) and against the account, whatever the address: after 10 wrong codes in a
row, the account's codes are refused for 15 minutes, then 30, doubling up to 24 hours
(`429`, `Retry-After`). A right code starts the count again. Passkeys are not counted.
Reaching a code step already takes the password, so this cannot lock a stranger out.

With `[security.mfa] staff = "required"`, staff (Admin or Manager role, or any staff
role) get no token without a second factor, and admin routes refuse tokens signed in
without one.

Configuration: `[security.mfa]` and `[security.webauthn]`
(see `docs/development/configuration-reference.md`). Without `encryption_key` there are
no authenticator apps; without `rp_id` there are no passkeys.

## Sign-in

### Password, then the second step

`POST /auth/login` answers as before for an account without a second factor. Otherwise:

```json
403 {"error": {"code": "second_factor_required", "message": "...",
               "challenge": "<uuid>", "methods": ["totp", "passkey", "recovery_code"]}}
```

The challenge lasts 5 minutes, takes 5 wrong answers, and works once.

| Request | Body | Answer |
|---|---|---|
| `POST /auth/login/second-factor` | `{challenge, code}` or `{challenge, recovery_code}` | the login answer (tokens) |
| `POST /auth/login/second-factor/passkey/options` | `{challenge}` | `{state_id, options}` |
| `POST /auth/login/second-factor/passkey/verify` | `{challenge, state_id, credential}` | the login answer |

`options` is the `PublicKeyCredential*Options` JSON (`options.publicKey` goes to
`PublicKeyCredential.parseRequestOptionsFromJSON`); `credential` is the browser's
`credential.toJSON()`.

Staff without a second factor, when it is required:

```json
403 {"error": {"code": "second_factor_enrolment_required", "message": "...",
               "setup_token": "<jwt, 15 minutes>", "methods": ["passkey", "totp"]}}
```

The setup token opens only the settings below, and adds the first factor only. Adding
it answers with `session` (the login answer) next to the recovery codes.

### A passkey alone

| Request | Body | Answer |
|---|---|---|
| `POST /auth/passkey/options` | `{}` | `{state_id, options}` (no `allowCredentials`: discoverable) |
| `POST /auth/passkey/verify` | `{state_id, credential, remember_me?}` | the login answer; 404 for an unknown passkey |

On a 404, the page calls `PublicKeyCredential.signalUnknownCredential` so the password
manager forgets the passkey.

### Tokens

Access tokens carry `amr` (RFC 8176: `pwd`, `otp`, `hwk`, `user`, and `mfa` after two
factors) and `auth_time` (the last proof of identity). Sessions remember `mfa`, so
refreshed tokens keep it; they carry no `auth_time`.

Access tokens also name their session (`sid`) and end with it. Re-verification
(`/auth/reauth*`) answers `401` for a token without one (issued before sessions were
named): sign in again. A setup token is backed by an `auth_challenges` row (`kind`
setup, its `jti`); a password change or reset, an admin's reset or the first factor
deletes it, and the token stops working.

## Settings of the signed-in account

Bearer: an access token, or the setup token where noted. Adding or removing a factor
needs a proof of identity in the last 5 minutes (`auth_time`); otherwise
`403 {"error": {"code": "reauth_required"}}`.

| Request | Needs | Answer |
|---|---|---|
| `GET /customers/me/security` | access or setup | `{totp, passkeys[], recovery_codes_left, second_factor_required, available[], rp_id, webauthn_user_id, recently_verified}` |
| `POST /customers/me/security/totp/setup` | recent; setup ok | `{secret, otpauth_uri}` (draw the URI as a QR code) |
| `POST /customers/me/security/totp/confirm` `{code}` | setup ok | `{recovery_codes?, session?}` |
| `DELETE /customers/me/security/totp` | recent | 204 |
| `POST /customers/me/security/passkeys/options` | recent; setup ok | `{state_id, options}` (resident key and user verification required) |
| `POST /customers/me/security/passkeys/verify` `{state_id, credential, name?}` | setup ok | `{passkey, recovery_codes?, session?}` |
| `PATCH /customers/me/security/passkeys/:id` `{name}` | access | 204 |
| `DELETE /customers/me/security/passkeys/:id` | recent | 204 |
| `POST /customers/me/security/recovery-codes` | recent | `{recovery_codes}` (the old ones stop working) |

Staff who must have a second factor cannot remove the last one (409
`second_factor_required_for_staff`).

When the first factor is added, the answer carries `recovery_codes` and a new
`session`: every session opened with the password alone ends (someone else may know the
password), so the apps keep the new tokens. An app setup left unconfirmed expires after
10 minutes. `rp_id` and `webauthn_user_id`
are for the WebAuthn Signal API (`signalAllAcceptedCredentials`,
`signalCurrentUserDetails`): always take `rp_id` from here, never from the page's host.

## Re-verification

| Request | Body | Answer |
|---|---|---|
| `POST /auth/reauth` | `{password}` (no second factor), else `{code}` or `{recovery_code}` | `{access_token, token_type, expires_in}` with `auth_time` = now; `amr` is the proof just given (a password alone is not `mfa`) |
| `POST /auth/reauth/passkey/options` | `{}` | `{state_id, options}` (the account's passkeys only) |
| `POST /auth/reauth/passkey/verify` | `{state_id, credential}` | as `/auth/reauth` |

A passkey of another account is refused here, even a valid one.

## Admin

`POST /admin/staff/:id/mfa/reset` (admins, recent proof of identity): removes the
account's passkeys, authenticator app and recovery codes, and ends its sessions (a lost
phone). The next sign-in asks for a new factor when one is required.

Answers of the sign-in and two-step routes carry `Cache-Control: no-store`.

## Security log

`security_events` records sign-ins (`detail.method`), wrong second steps, factors added
and removed, recovery codes used or renewed, re-verifications and resets (`actor_id`
for a reset by an admin), with the client IP.

## E-mails to the account's owner

When the notification service is configured, the owner gets an e-mail (template
`security_notice_html`, section `[security_notice]` of the e-mail catalogues, in the
customer's language) when the password is changed or reset, an authenticator app is
turned on or off, a passkey is added or removed, a recovery code is used, new recovery
codes are made, or an admin resets the account's factors (`events::NOTIFIED`). It says
what changed and when, links to `<storefront_url>/account/security`, and tells the
owner what to do if it was not them. The greeting keeps only the letters, spaces,
hyphens and apostrophes of the name (whoever holds the account cannot write a message
into it). At most one notice per account and kind every 10 minutes, per process. It is
sent in the background: a failed e-mail does not fail the request.

## Storage

- `customer_passkeys`: the verified credential (public key, signature counter), name,
  AAGUID (display only: it is not attested), backup flags, last use.
- `customer_totp`: the secret, AES-256-GCM with `encryption_key`; the last accepted
  step.
- `mfa_recovery_codes`: Argon2id of each code, with its own random salt.
- `mfa_lockouts`: wrong codes in a row per account, and until when codes are refused.
- `auth_challenges`: one-time second steps and WebAuthn ceremony states (5 minutes).
- `customers.webauthn_user_id`: a random id given to passkey providers instead of the
  customer id or e-mail.
