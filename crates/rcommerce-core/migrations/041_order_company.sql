-- The business account an order was placed for (NULL: a retail order), so
-- reports split trade and retail sales without reading every line. Checkout
-- sets it with the order's pricing snapshot (services::pricing_history).
ALTER TABLE orders
    ADD COLUMN IF NOT EXISTS company_id UUID REFERENCES companies(id) ON DELETE SET NULL;

-- Existing orders: the company of their pricing snapshot, then of their
-- invoice, then the customer's company today
UPDATE orders o SET company_id = p.company_id
FROM (
    SELECT DISTINCT ON (order_id) order_id, company_id
    FROM order_line_pricing
    WHERE company_id IS NOT NULL
    ORDER BY order_id, created_at
) p
WHERE p.order_id = o.id AND o.company_id IS NULL;

UPDATE orders o SET company_id = i.company_id
FROM invoices i
WHERE i.order_id = o.id AND i.company_id IS NOT NULL AND o.company_id IS NULL;

UPDATE orders o SET company_id = m.company_id
FROM company_members m
WHERE m.customer_id = o.customer_id AND o.company_id IS NULL;

CREATE INDEX IF NOT EXISTS orders_company_idx
    ON orders (company_id, created_at) WHERE company_id IS NOT NULL;
