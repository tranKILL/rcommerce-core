-- Marketing consent: the proof of each choice a customer makes about commercial
-- e-mails (GDPR art. 7). `customers.accepts_marketing` holds the current one; the
-- unused `marketing_opt_in` and `customer_notification_preferences.marketing_emails`
-- columns are left as they are.
CREATE TABLE IF NOT EXISTS marketing_consents (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id UUID NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    granted BOOLEAN NOT NULL,
    source VARCHAR(16) NOT NULL CHECK (source IN ('register', 'account', 'unsubscribe')),
    -- the version of the text the customer was shown
    wording_version VARCHAR(32),
    ip VARCHAR(45),
    user_agent VARCHAR(500),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_marketing_consents_customer
    ON marketing_consents (customer_id, created_at DESC);
