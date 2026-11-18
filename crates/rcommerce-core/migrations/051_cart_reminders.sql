-- Reminders of carts left without an order (`[carts] reminder_after_hours`).
-- A cart's idle period starts at its last change by the customer
-- (`period_start`); each step of the schedule is sent once per period. The
-- cart is repriced before each reminder, which moves `carts.updated_at`: that
-- value is kept in `cart_seen_at`, so a cart still at it has not been changed
-- by the customer since and stays in the same period.
CREATE TABLE IF NOT EXISTS cart_reminders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    cart_id UUID NOT NULL REFERENCES carts(id) ON DELETE CASCADE,
    customer_id UUID NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    -- 1 for the first delay of the schedule, 2 for the second...
    step SMALLINT NOT NULL CHECK (step > 0),
    period_start TIMESTAMPTZ NOT NULL,
    cart_seen_at TIMESTAMPTZ NOT NULL,
    sent_at TIMESTAMPTZ,
    -- failed sends; the step is given up after a few
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (cart_id, period_start, step)
);

-- At most one reminder per customer in a while, and the figures by date
CREATE INDEX IF NOT EXISTS idx_cart_reminders_customer_sent
    ON cart_reminders (customer_id, sent_at DESC);
CREATE INDEX IF NOT EXISTS idx_cart_reminders_sent
    ON cart_reminders (sent_at) WHERE sent_at IS NOT NULL;
