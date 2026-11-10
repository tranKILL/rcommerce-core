-- Price lists: negotiated prices (excluding VAT) for one customer group or one
-- company, possibly limited in time. When several rules apply, the lowest
-- price wins.

CREATE TABLE IF NOT EXISTS price_lists (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name VARCHAR(255) NOT NULL,
    customer_group_id UUID REFERENCES customer_groups(id) ON DELETE CASCADE,
    company_id UUID REFERENCES companies(id) ON DELETE CASCADE,
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    starts_at TIMESTAMPTZ,
    ends_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- For exactly one group or one company
    CHECK ((customer_group_id IS NULL) <> (company_id IS NULL)),
    CHECK (ends_at IS NULL OR starts_at IS NULL OR ends_at > starts_at)
);

CREATE INDEX IF NOT EXISTS idx_price_lists_group ON price_lists (customer_group_id) WHERE customer_group_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_price_lists_company ON price_lists (company_id) WHERE company_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS price_list_rules (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    price_list_id UUID NOT NULL REFERENCES price_lists(id) ON DELETE CASCADE,
    -- Every product, a category (with its sub-categories), a product or a variant
    scope VARCHAR(10) NOT NULL CHECK (scope IN ('all', 'category', 'product', 'variant')),
    target_id UUID,
    adjustment VARCHAR(12) NOT NULL CHECK (adjustment IN ('fixed', 'percent_off', 'amount_off')),
    value NUMERIC(12, 4) NOT NULL CHECK (value >= 0),
    -- The rule applies from this quantity on a cart line (quantity tiers)
    min_quantity INTEGER NOT NULL DEFAULT 1 CHECK (min_quantity >= 1),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ((scope = 'all') = (target_id IS NULL)),
    CHECK (adjustment <> 'percent_off' OR value <= 100)
);

CREATE INDEX IF NOT EXISTS idx_price_list_rules_list ON price_list_rules (price_list_id);

-- The list a cart line's price comes from; such lines are repriced whenever
-- the cart changes (and at checkout), so they follow the customer's lists
ALTER TABLE cart_items ADD COLUMN IF NOT EXISTS price_list_id UUID;
