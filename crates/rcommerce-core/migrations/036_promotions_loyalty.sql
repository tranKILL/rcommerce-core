-- Promotions, the price history behind the "lowest price of the last 30 days"
-- shown with a reduction, and loyalty points.

-- A promotion. Catalogue kinds lower the price of what they target
-- (percent_off, amount_off, fixed_price); cart kinds act on the cart
-- (buy_x_get_y, spend_tiers, free_delivery, gift).
CREATE TABLE IF NOT EXISTS promotions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name VARCHAR(150) NOT NULL,
    kind VARCHAR(20) NOT NULL CHECK (kind IN (
        'percent_off', 'amount_off', 'fixed_price',
        'buy_x_get_y', 'spend_tiers', 'free_delivery', 'gift')),
    -- who gets it: retail customers (and guests), trade customers, or both
    audience VARCHAR(10) NOT NULL DEFAULT 'customer'
        CHECK (audience IN ('customer', 'pro', 'both')),
    -- what it targets (catalogue kinds, buy_x_get_y; cart kinds count only
    -- the targeted lines towards their thresholds)
    scope VARCHAR(10) NOT NULL DEFAULT 'all'
        CHECK (scope IN ('all', 'category', 'product', 'variant')),
    target_id UUID,
    -- percent (percent_off; buy_x_get_y: discount on the "get" units, 100 = free),
    -- amount (amount_off) or price (fixed_price), excluding VAT
    value NUMERIC(12, 2),
    buy_quantity INTEGER CHECK (buy_quantity IS NULL OR buy_quantity >= 1),
    get_quantity INTEGER CHECK (get_quantity IS NULL OR get_quantity >= 1),
    -- spend_tiers: [{"min_amount": "50", "percent": "5"}, ...]
    tiers JSONB,
    -- conditions: the targeted lines' total and quantity it needs, first
    -- order only, how many orders may use it (all customers, one customer)
    min_amount NUMERIC(12, 2) CHECK (min_amount IS NULL OR min_amount >= 0),
    min_quantity INTEGER CHECK (min_quantity IS NULL OR min_quantity >= 1),
    first_order_only BOOLEAN NOT NULL DEFAULT false,
    max_uses INTEGER CHECK (max_uses IS NULL OR max_uses >= 1),
    max_uses_per_customer INTEGER CHECK (max_uses_per_customer IS NULL OR max_uses_per_customer >= 1),
    -- whether it adds up with other combinable promotions on the same lines
    combinable BOOLEAN NOT NULL DEFAULT false,
    gift_product_id UUID REFERENCES products(id) ON DELETE SET NULL,
    gift_quantity INTEGER NOT NULL DEFAULT 1 CHECK (gift_quantity >= 1),
    starts_at TIMESTAMPTZ,
    ends_at TIMESTAMPTZ,
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (scope = 'all' OR target_id IS NOT NULL),
    CHECK (ends_at IS NULL OR starts_at IS NULL OR ends_at > starts_at)
);
CREATE INDEX IF NOT EXISTS promotions_live_idx ON promotions (is_active, starts_at, ends_at);

-- The orders that used a promotion (usage limits)
CREATE TABLE IF NOT EXISTS promotion_uses (
    promotion_id UUID NOT NULL REFERENCES promotions(id) ON DELETE CASCADE,
    order_id UUID NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    customer_id UUID,
    email VARCHAR(255),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (promotion_id, order_id)
);
CREATE INDEX IF NOT EXISTS promotion_uses_customer_idx ON promotion_uses (promotion_id, customer_id);

-- The catalogue promotions that price a cart line, and a cart's applied
-- promotions and redeemed points
ALTER TABLE cart_items ADD COLUMN IF NOT EXISTS promotion_ids UUID[] NOT NULL DEFAULT '{}';
ALTER TABLE carts ADD COLUMN IF NOT EXISTS promotions JSONB NOT NULL DEFAULT '[]';
ALTER TABLE carts ADD COLUMN IF NOT EXISTS loyalty_points INTEGER NOT NULL DEFAULT 0;

-- Every catalogue price a product or variant had, from when
CREATE TABLE IF NOT EXISTS price_history (
    id BIGSERIAL PRIMARY KEY,
    product_id UUID NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    variant_id UUID REFERENCES product_variants(id) ON DELETE CASCADE,
    price NUMERIC(20, 4) NOT NULL,
    valid_from TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS price_history_item_idx ON price_history (product_id, variant_id, valid_from);

CREATE OR REPLACE FUNCTION rc_record_product_price() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' OR NEW.price IS DISTINCT FROM OLD.price THEN
        INSERT INTO price_history (product_id, variant_id, price) VALUES (NEW.id, NULL, NEW.price);
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS products_price_history ON products;
CREATE TRIGGER products_price_history AFTER INSERT OR UPDATE OF price ON products
    FOR EACH ROW EXECUTE FUNCTION rc_record_product_price();

CREATE OR REPLACE FUNCTION rc_record_variant_price() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.price IS NOT NULL AND (TG_OP = 'INSERT' OR NEW.price IS DISTINCT FROM OLD.price) THEN
        INSERT INTO price_history (product_id, variant_id, price) VALUES (NEW.product_id, NEW.id, NEW.price);
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS variants_price_history ON product_variants;
CREATE TRIGGER variants_price_history AFTER INSERT OR UPDATE OF price ON product_variants
    FOR EACH ROW EXECUTE FUNCTION rc_record_variant_price();

-- Today's prices start the history
INSERT INTO price_history (product_id, variant_id, price)
SELECT id, NULL, price FROM products
WHERE NOT EXISTS (SELECT 1 FROM price_history h WHERE h.product_id = products.id AND h.variant_id IS NULL);
INSERT INTO price_history (product_id, variant_id, price)
SELECT product_id, id, price FROM product_variants v
WHERE price IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM price_history h WHERE h.variant_id = v.id);

-- Loyalty points: one settings row, and a ledger per customer
CREATE TABLE IF NOT EXISTS loyalty_settings (
    id BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    enabled BOOLEAN NOT NULL DEFAULT false,
    audience VARCHAR(10) NOT NULL DEFAULT 'customer'
        CHECK (audience IN ('customer', 'pro', 'both')),
    -- points per euro spent (excluding VAT, delivery and discounts)
    points_per_euro NUMERIC(8, 2) NOT NULL DEFAULT 1 CHECK (points_per_euro > 0),
    -- what one point is worth when redeemed, in euros
    point_value NUMERIC(8, 4) NOT NULL DEFAULT 0.05 CHECK (point_value > 0),
    min_redeem INTEGER NOT NULL DEFAULT 100 CHECK (min_redeem >= 1),
    -- points lapse after this many months without earning or redeeming (NULL: never)
    expiry_months INTEGER CHECK (expiry_months IS NULL OR expiry_months >= 1),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
INSERT INTO loyalty_settings (id) VALUES (true) ON CONFLICT DO NOTHING;

CREATE TABLE IF NOT EXISTS loyalty_entries (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id UUID NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    points INTEGER NOT NULL,
    kind VARCHAR(10) NOT NULL CHECK (kind IN ('earn', 'redeem', 'reverse', 'refund', 'adjust', 'expire')),
    order_id UUID REFERENCES orders(id) ON DELETE SET NULL,
    note TEXT,
    created_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS loyalty_entries_customer_idx ON loyalty_entries (customer_id, created_at);
-- once per order: the award, the redemption, and their undoing
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_entries_order_kind_idx
    ON loyalty_entries (order_id, kind) WHERE order_id IS NOT NULL AND kind IN ('earn', 'redeem', 'reverse', 'refund');
