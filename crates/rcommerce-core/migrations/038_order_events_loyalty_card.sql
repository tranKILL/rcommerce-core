-- An order's history (who did what, when), and a loyalty card number per
-- customer, shown as a QR code at the counter

CREATE TABLE IF NOT EXISTS order_events (
    id BIGSERIAL PRIMARY KEY,
    order_id UUID NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    -- placed, paid, preparing, ready, collected, dispatched, delivered,
    -- cancelled, refunded, invoiced
    kind VARCHAR(20) NOT NULL,
    actor_role VARCHAR(10) NOT NULL DEFAULT 'system'
        CHECK (actor_role IN ('customer', 'staff', 'system')),
    actor_id UUID,
    actor_name VARCHAR(255),
    detail JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS order_events_order_idx ON order_events (order_id, created_at);

-- The history of orders placed before: what their columns and metadata tell
DO $$
BEGIN
IF NOT EXISTS (SELECT 1 FROM order_events) THEN
    INSERT INTO order_events (order_id, kind, actor_role, actor_id, detail, created_at)
    SELECT id, 'placed', 'customer', customer_id, '{}', created_at FROM orders WHERE NOT draft;
    INSERT INTO order_events (order_id, kind, detail, created_at)
    SELECT id, 'ready', jsonb_build_object('location', metadata->'pickup'->>'location_name'),
           (metadata->'pickup'->>'ready_marked_at')::timestamptz
    FROM orders WHERE metadata->'pickup'->>'ready_marked_at' IS NOT NULL;
    INSERT INTO order_events (order_id, kind, detail, created_at)
    SELECT id, 'collected', jsonb_build_object('collected_by', metadata->'pickup'->>'collected_by',
                                               'location', metadata->'pickup'->>'location_name'),
           (metadata->'pickup'->>'collected_at')::timestamptz
    FROM orders WHERE metadata->'pickup'->>'collected_at' IS NOT NULL;
    INSERT INTO order_events (order_id, kind, detail, created_at)
    SELECT id, 'dispatched', jsonb_build_object('driver', metadata->'delivery'->>'driver'),
           (metadata->'delivery'->>'dispatched_at')::timestamptz
    FROM orders WHERE metadata->'delivery'->>'dispatched_at' IS NOT NULL;
    INSERT INTO order_events (order_id, kind, detail, created_at)
    SELECT id, 'delivered', jsonb_build_object('received_by', metadata->'delivery'->>'received_by'),
           (metadata->'delivery'->>'delivered_at')::timestamptz
    FROM orders WHERE metadata->'delivery'->>'delivered_at' IS NOT NULL;
    INSERT INTO order_events (order_id, kind, detail, created_at)
    SELECT id, 'cancelled', jsonb_build_object('reason', metadata->>'cancellation_reason'), cancelled_at
    FROM orders WHERE cancelled_at IS NOT NULL;
END IF;
END $$;

-- A 13-digit number (EAN-13 layout, in-store range 29...), given on first use
ALTER TABLE customers ADD COLUMN IF NOT EXISTS loyalty_card VARCHAR(13);
CREATE UNIQUE INDEX IF NOT EXISTS customers_loyalty_card_idx ON customers (loyalty_card) WHERE loyalty_card IS NOT NULL;
