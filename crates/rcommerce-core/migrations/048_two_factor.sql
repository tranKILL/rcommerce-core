-- Two-step sign-in: passkeys (WebAuthn), authenticator apps (TOTP), recovery codes,
-- one-time sign-in challenges, and a log of security events.

-- The account's id towards passkey providers: random, so the customer id and e-mail stay private
ALTER TABLE customers ADD COLUMN IF NOT EXISTS webauthn_user_id UUID UNIQUE;

CREATE TABLE IF NOT EXISTS customer_passkeys (
    -- base64url credential id, as the browser reports it
    credential_id TEXT PRIMARY KEY,
    customer_id UUID NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    -- the verified credential (public key, signature counter, flags)
    passkey JSONB NOT NULL,
    name VARCHAR(100) NOT NULL,
    -- authenticator model, for the provider's name and icon only
    aaguid UUID,
    backup_eligible BOOLEAN NOT NULL DEFAULT false,
    backed_up BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_customer_passkeys_customer ON customer_passkeys (customer_id);

CREATE TABLE IF NOT EXISTS customer_totp (
    customer_id UUID PRIMARY KEY REFERENCES customers(id) ON DELETE CASCADE,
    -- nonce || AES-256-GCM ciphertext of the shared secret
    secret_enc BYTEA NOT NULL,
    -- NULL until the customer proves the app works by typing a code
    confirmed_at TIMESTAMPTZ,
    -- the last 30-second step accepted: a code is never accepted twice
    last_step BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS mfa_recovery_codes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id UUID NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL,
    used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_mfa_recovery_codes_customer ON mfa_recovery_codes (customer_id);

-- Wrong second steps per account, whatever the address: after 10 in a row, codes are
-- refused for a growing while (passkeys still work)
CREATE TABLE IF NOT EXISTS mfa_lockouts (
    customer_id UUID PRIMARY KEY REFERENCES customers(id) ON DELETE CASCADE,
    failures INT NOT NULL DEFAULT 0,
    locked_until TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- One-time steps of a sign-in or a passkey ceremony: the second step after a password,
-- and the WebAuthn state between the options and the browser's answer
CREATE TABLE IF NOT EXISTS auth_challenges (
    id UUID PRIMARY KEY,
    customer_id UUID REFERENCES customers(id) ON DELETE CASCADE,
    kind VARCHAR(32) NOT NULL,
    data JSONB NOT NULL DEFAULT '{}'::jsonb,
    attempts INT NOT NULL DEFAULT 0,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_auth_challenges_expires ON auth_challenges (expires_at);

-- Sessions remember whether the sign-in used a second factor: refreshed tokens keep it
ALTER TABLE auth_sessions ADD COLUMN IF NOT EXISTS mfa BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS security_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id UUID REFERENCES customers(id) ON DELETE SET NULL,
    -- the staff member who acted on someone else's account, if any
    actor_id UUID REFERENCES customers(id) ON DELETE SET NULL,
    kind VARCHAR(64) NOT NULL,
    ip VARCHAR(64),
    detail JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_security_events_customer ON security_events (customer_id, created_at DESC);
