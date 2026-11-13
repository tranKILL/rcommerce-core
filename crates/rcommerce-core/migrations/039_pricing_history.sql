-- Price and promotion history: what each order line was charged and why,
-- what each promotion saved each order, and every negotiated price rule a
-- price list had, from when to when. Kept apart from the live tables so
-- promotions, price lists and orders stay light.

-- A cart line's price before catalogue promotions: its price list's, else
-- the catalogue price
ALTER TABLE cart_items ADD COLUMN IF NOT EXISTS negotiated_price NUMERIC(20, 4);

-- One row per order line, written when the order is placed
CREATE TABLE IF NOT EXISTS order_line_pricing (
    order_item_id UUID PRIMARY KEY REFERENCES order_items(id) ON DELETE CASCADE,
    order_id UUID NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    product_id UUID,
    variant_id UUID,
    customer_id UUID,
    company_id UUID,
    quantity INTEGER NOT NULL,
    -- catalogue price, price before catalogue promotions, price charged
    base_price NUMERIC(20, 4) NOT NULL,
    negotiated_price NUMERIC(20, 4) NOT NULL,
    unit_price NUMERIC(20, 4) NOT NULL,
    -- the line's share of cart promotions, coupon and points
    discount NUMERIC(20, 4) NOT NULL DEFAULT 0,
    cost_price NUMERIC(20, 4),
    price_list_id UUID,
    price_list_name VARCHAR(255),
    -- [{id, name, kind, saving}]: the promotions behind the price, by name in
    -- case they are deleted later
    promotions JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS order_line_pricing_product_idx ON order_line_pricing (product_id, created_at DESC);
CREATE INDEX IF NOT EXISTS order_line_pricing_customer_idx ON order_line_pricing (customer_id, created_at DESC)
    WHERE customer_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS order_line_pricing_company_idx ON order_line_pricing (company_id, created_at DESC)
    WHERE company_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS order_line_pricing_order_idx ON order_line_pricing (order_id);

-- Earlier orders: what is known (the price charged, the catalogue price of
-- the day when recorded)
INSERT INTO order_line_pricing (order_item_id, order_id, product_id, variant_id, customer_id, quantity,
                                base_price, negotiated_price, unit_price, created_at)
SELECT oi.id, o.id, oi.product_id, oi.variant_id, o.customer_id, oi.quantity,
       GREATEST(COALESCE(h.price, oi.price), oi.price), GREATEST(COALESCE(h.price, oi.price), oi.price),
       oi.price, o.created_at
FROM order_items oi
JOIN orders o ON o.id = oi.order_id
LEFT JOIN LATERAL (
    SELECT ph.price FROM price_history ph
    WHERE ph.product_id = oi.product_id AND ph.variant_id IS NOT DISTINCT FROM oi.variant_id
      AND ph.valid_from <= o.created_at
    ORDER BY ph.valid_from DESC LIMIT 1
) h ON true
WHERE NOT COALESCE(oi.is_bundle_component, false)
ON CONFLICT DO NOTHING;

-- What each promotion saved an order, with its name and kind kept: a
-- deleted promotion's uses stay in the history
ALTER TABLE promotion_uses ADD COLUMN IF NOT EXISTS name VARCHAR(255);
ALTER TABLE promotion_uses ADD COLUMN IF NOT EXISTS kind VARCHAR(30);
ALTER TABLE promotion_uses ADD COLUMN IF NOT EXISTS discount NUMERIC(20, 4) NOT NULL DEFAULT 0;
ALTER TABLE promotion_uses DROP CONSTRAINT IF EXISTS promotion_uses_promotion_id_fkey;
UPDATE promotion_uses u SET name = p.name, kind = p.kind
FROM promotions p WHERE p.id = u.promotion_id AND u.name IS NULL;

-- Every rule a price list had, with the list as it was: `valid_to` is NULL
-- for the rules in force
CREATE TABLE IF NOT EXISTS price_list_rule_history (
    id BIGSERIAL PRIMARY KEY,
    rule_id UUID NOT NULL,
    price_list_id UUID NOT NULL,
    price_list_name VARCHAR(255) NOT NULL,
    customer_group_id UUID,
    company_id UUID,
    list_active BOOLEAN NOT NULL,
    list_starts_at TIMESTAMPTZ,
    list_ends_at TIMESTAMPTZ,
    scope VARCHAR(10) NOT NULL,
    target_id UUID,
    adjustment VARCHAR(12) NOT NULL,
    value NUMERIC(12, 4) NOT NULL,
    min_quantity INTEGER NOT NULL,
    valid_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_to TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS price_list_rule_history_target_idx ON price_list_rule_history (target_id, valid_from);
CREATE INDEX IF NOT EXISTS price_list_rule_history_company_idx ON price_list_rule_history (company_id)
    WHERE company_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS price_list_rule_history_group_idx ON price_list_rule_history (customer_group_id)
    WHERE customer_group_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS price_list_rule_history_open_idx ON price_list_rule_history (rule_id)
    WHERE valid_to IS NULL;

-- Open a history row for each current rule of `lists` or of `rules`
CREATE OR REPLACE FUNCTION rc_open_rule_history(lists UUID[], rules UUID[]) RETURNS void
LANGUAGE sql AS $$
    INSERT INTO price_list_rule_history (rule_id, price_list_id, price_list_name, customer_group_id, company_id,
                                         list_active, list_starts_at, list_ends_at, scope, target_id,
                                         adjustment, value, min_quantity)
    SELECT r.id, l.id, l.name, l.customer_group_id, l.company_id, l.is_active, l.starts_at, l.ends_at,
           r.scope, r.target_id, r.adjustment, r.value, r.min_quantity
    FROM price_list_rules r JOIN price_lists l ON l.id = r.price_list_id
    WHERE l.id = ANY(lists) OR r.id = ANY(rules)
$$;

CREATE OR REPLACE FUNCTION rc_record_price_rule() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'INSERT' THEN
        UPDATE price_list_rule_history SET valid_to = NOW() WHERE rule_id = OLD.id AND valid_to IS NULL;
    END IF;
    IF TG_OP <> 'DELETE' THEN
        PERFORM rc_open_rule_history('{}', ARRAY[NEW.id]);
    END IF;
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS price_list_rules_history ON price_list_rules;
CREATE TRIGGER price_list_rules_history AFTER INSERT OR UPDATE OR DELETE ON price_list_rules
    FOR EACH ROW EXECUTE FUNCTION rc_record_price_rule();

-- A list renamed, switched, re-dated or given to someone else: its rules'
-- rows start again
CREATE OR REPLACE FUNCTION rc_record_price_list() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF ROW(NEW.name, NEW.is_active, NEW.starts_at, NEW.ends_at, NEW.company_id, NEW.customer_group_id)
       IS DISTINCT FROM ROW(OLD.name, OLD.is_active, OLD.starts_at, OLD.ends_at, OLD.company_id, OLD.customer_group_id)
    THEN
        UPDATE price_list_rule_history SET valid_to = NOW() WHERE price_list_id = NEW.id AND valid_to IS NULL;
        PERFORM rc_open_rule_history(ARRAY[NEW.id], '{}');
    END IF;
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS price_lists_history ON price_lists;
CREATE TRIGGER price_lists_history AFTER UPDATE ON price_lists
    FOR EACH ROW EXECUTE FUNCTION rc_record_price_list();

-- Today's rules start the history, from when they were created
INSERT INTO price_list_rule_history (rule_id, price_list_id, price_list_name, customer_group_id, company_id,
                                     list_active, list_starts_at, list_ends_at, scope, target_id,
                                     adjustment, value, min_quantity, valid_from)
SELECT r.id, l.id, l.name, l.customer_group_id, l.company_id, l.is_active, l.starts_at, l.ends_at,
       r.scope, r.target_id, r.adjustment, r.value, r.min_quantity, r.created_at
FROM price_list_rules r JOIN price_lists l ON l.id = r.price_list_id
WHERE NOT EXISTS (SELECT 1 FROM price_list_rule_history h WHERE h.rule_id = r.id);
