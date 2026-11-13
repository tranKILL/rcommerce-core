-- A fixed amount off the order (once per order, on the lines the promotion
-- targets), and what catalogue promotions take off a cart's lines, shown
-- with its totals (the line prices already include it)
ALTER TABLE promotions DROP CONSTRAINT IF EXISTS promotions_kind_check;
ALTER TABLE promotions ADD CONSTRAINT promotions_kind_check CHECK (kind IN (
    'percent_off', 'amount_off', 'fixed_price',
    'buy_x_get_y', 'spend_tiers', 'order_amount_off', 'free_delivery', 'gift'));

ALTER TABLE carts ADD COLUMN IF NOT EXISTS catalogue_promotions JSONB NOT NULL DEFAULT '[]';

-- One default delivery address and one default billing address per
-- customer: earlier duplicates keep the most recently saved one
UPDATE addresses a SET is_default_shipping = false
WHERE a.is_default_shipping AND EXISTS (
    SELECT 1 FROM addresses b WHERE b.customer_id = a.customer_id AND b.is_default_shipping
      AND (b.updated_at, b.id) > (a.updated_at, a.id));
UPDATE addresses a SET is_default_billing = false
WHERE a.is_default_billing AND EXISTS (
    SELECT 1 FROM addresses b WHERE b.customer_id = a.customer_id AND b.is_default_billing
      AND (b.updated_at, b.id) > (a.updated_at, a.id));
CREATE UNIQUE INDEX IF NOT EXISTS addresses_one_default_shipping ON addresses (customer_id) WHERE is_default_shipping;
CREATE UNIQUE INDEX IF NOT EXISTS addresses_one_default_billing ON addresses (customer_id) WHERE is_default_billing;
