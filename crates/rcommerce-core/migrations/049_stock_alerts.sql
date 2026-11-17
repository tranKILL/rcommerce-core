-- Stock alerts: tell a signed-in customer when a product is in stock at a branch (or
-- at any branch) in the quantity they want. Each alert is sent once, then closed.
CREATE TABLE IF NOT EXISTS stock_alerts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id UUID NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    product_id UUID NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    variant_id UUID REFERENCES product_variants(id) ON DELETE CASCADE,
    -- none: any public branch
    location_id UUID REFERENCES inventory_locations(id) ON DELETE CASCADE,
    quantity INT NOT NULL DEFAULT 1 CHECK (quantity BETWEEN 1 AND 9999),
    -- failed sends: the alert closes after 5, and alerts tried less go first
    attempts INT NOT NULL DEFAULT 0,
    -- after a failed send, not before (15 minutes, doubling)
    next_attempt_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    notified_at TIMESTAMPTZ,
    cancelled_at TIMESTAMPTZ
);

-- One open alert per customer, product, variant and branch
CREATE UNIQUE INDEX IF NOT EXISTS idx_stock_alerts_open_once ON stock_alerts (
    customer_id,
    product_id,
    COALESCE(variant_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(location_id, '00000000-0000-0000-0000-000000000000'::uuid)
) WHERE notified_at IS NULL AND cancelled_at IS NULL;

-- The check job reads the open alerts, tried less and oldest first
CREATE INDEX IF NOT EXISTS idx_stock_alerts_open ON stock_alerts (attempts, created_at)
    WHERE notified_at IS NULL AND cancelled_at IS NULL;
