-- Point of sale: till sessions (a cashier at a branch, from the opening float
-- to the count at closing and its Z report), the money each session takes in
-- or pays out, card terminal transactions, and manual discounts on basket
-- lines (given at the till, within the staff member's cap).

CREATE TABLE IF NOT EXISTS till_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    location_id UUID NOT NULL REFERENCES inventory_locations(id),
    cashier_id UUID NOT NULL REFERENCES customers(id),
    opening_float NUMERIC(12, 2) NOT NULL CHECK (opening_float >= 0),
    opened_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at TIMESTAMPTZ,
    -- at closing: cash counted, cash expected from the movements, the gap
    counted_cash NUMERIC(12, 2),
    expected_cash NUMERIC(12, 2),
    difference NUMERIC(12, 2),
    -- the branch's Z report number, given at closing
    z_number INTEGER,
    notes TEXT,
    UNIQUE (location_id, z_number)
);

-- One open session per cashier
CREATE UNIQUE INDEX IF NOT EXISTS till_sessions_one_open
    ON till_sessions (cashier_id) WHERE closed_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_till_sessions_location
    ON till_sessions (location_id, opened_at DESC);

-- Money in (positive) or out (negative) of a session, by method
CREATE TABLE IF NOT EXISTS till_movements (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES till_sessions(id),
    kind VARCHAR(16) NOT NULL CHECK (kind IN ('sale', 'refund', 'cash_in', 'cash_out')),
    method VARCHAR(16) NOT NULL CHECK (method IN ('cash', 'card')),
    amount NUMERIC(12, 2) NOT NULL,
    order_id UUID REFERENCES orders(id) ON DELETE SET NULL,
    reason TEXT,
    staff_id UUID REFERENCES customers(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_till_movements_session
    ON till_movements (session_id, created_at);
CREATE INDEX IF NOT EXISTS idx_till_movements_order
    ON till_movements (order_id) WHERE order_id IS NOT NULL;

-- Payments and refunds sent to a card terminal, and how they ended
CREATE TABLE IF NOT EXISTS terminal_transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES till_sessions(id),
    provider VARCHAR(32) NOT NULL,
    reference VARCHAR(100) NOT NULL,
    kind VARCHAR(16) NOT NULL CHECK (kind IN ('payment', 'refund')),
    amount NUMERIC(12, 2) NOT NULL CHECK (amount > 0),
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'approved', 'declined', 'cancelled')),
    -- the sale or return it paid, once used
    order_id UUID REFERENCES orders(id) ON DELETE SET NULL,
    raw JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (provider, reference)
);

-- A manual discount on a basket line: the percentage given, why, and what it
-- takes off each unit once the line is priced
ALTER TABLE cart_items
    ADD COLUMN IF NOT EXISTS manual_discount_percent NUMERIC(5, 2)
        CHECK (manual_discount_percent > 0 AND manual_discount_percent <= 100),
    ADD COLUMN IF NOT EXISTS manual_discount_reason TEXT,
    ADD COLUMN IF NOT EXISTS manual_discount NUMERIC(12, 2) NOT NULL DEFAULT 0;

-- Cash taken at the till (card payments there are recorded as debit cards)
ALTER TYPE payment_method_type ADD VALUE IF NOT EXISTS 'cash';
