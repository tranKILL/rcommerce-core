-- Web orders sent to the shop that prepares them: who receives them, the
-- paper of the order slip, and the shop's photo for the storefront
CREATE TABLE IF NOT EXISTS location_order_recipients (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    location_id UUID NOT NULL REFERENCES inventory_locations(id) ON DELETE CASCADE,
    email VARCHAR(254) NOT NULL,
    customer_id UUID REFERENCES customers(id) ON DELETE CASCADE,
    locale VARCHAR(10),
    active BOOLEAN NOT NULL DEFAULT true,
    -- the staff member who set the list this address is in
    added_by UUID REFERENCES customers(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_location_order_recipients_email
    ON location_order_recipients (location_id, lower(email));

ALTER TABLE inventory_locations
    ADD COLUMN IF NOT EXISTS image_url TEXT,
    ADD COLUMN IF NOT EXISTS slip_format VARCHAR(10) NOT NULL DEFAULT 'a4'
        CHECK (slip_format IN ('a4', 'ticket80')),
    -- the last test e-mail to the shop's addresses (at most one a minute)
    ADD COLUMN IF NOT EXISTS order_test_sent_at TIMESTAMPTZ;

-- The orders the shop-order job still has to send
CREATE INDEX IF NOT EXISTS idx_orders_shop_notice ON orders (created_at)
    WHERE metadata->>'shop_notified' IS NULL AND NOT draft;
