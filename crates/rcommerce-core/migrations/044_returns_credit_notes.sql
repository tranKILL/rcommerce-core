-- Returns of goods already handed over: what came back (and whether it went
-- back on the shelf), the money refunded by method, the points taken back,
-- and the credit note issued against the order's invoice.

CREATE TABLE IF NOT EXISTS order_returns (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id UUID NOT NULL REFERENCES orders(id),
    -- the till session that took it back, and its branch
    session_id UUID REFERENCES till_sessions(id),
    location_id UUID REFERENCES inventory_locations(id),
    staff_id UUID REFERENCES customers(id) ON DELETE SET NULL,
    reason TEXT NOT NULL,
    -- excl. VAT after the returned share of the order's discount
    subtotal NUMERIC(12, 2) NOT NULL,
    tax_total NUMERIC(12, 2) NOT NULL,
    total NUMERIC(12, 2) NOT NULL,
    -- the returned share of the order's discount (coupon, points, promotions)
    discount NUMERIC(12, 2) NOT NULL DEFAULT 0,
    refund_online NUMERIC(12, 2) NOT NULL DEFAULT 0,
    refund_card NUMERIC(12, 2) NOT NULL DEFAULT 0,
    refund_cash NUMERIC(12, 2) NOT NULL DEFAULT 0,
    -- credited but not paid back (the order was not paid in full)
    not_refunded NUMERIC(12, 2) NOT NULL DEFAULT 0,
    points_taken_back INTEGER NOT NULL DEFAULT 0,
    points_given_back INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_order_returns_order ON order_returns (order_id);
CREATE INDEX IF NOT EXISTS idx_order_returns_session ON order_returns (session_id) WHERE session_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS order_return_lines (
    return_id UUID NOT NULL REFERENCES order_returns(id) ON DELETE CASCADE,
    order_item_id UUID NOT NULL REFERENCES order_items(id),
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    -- back in stock at the branch (false: damaged, thrown away)
    restocked BOOLEAN NOT NULL,
    -- quantity × unit price excl. VAT, before the discount share
    net NUMERIC(12, 2) NOT NULL,
    PRIMARY KEY (return_id, order_item_id)
);
CREATE INDEX IF NOT EXISTS idx_order_return_lines_item ON order_return_lines (order_item_id);

-- A credit note names the invoice it corrects and the return it comes from
ALTER TABLE invoices
    ADD COLUMN IF NOT EXISTS credited_invoice_id UUID REFERENCES invoices(id),
    ADD COLUMN IF NOT EXISTS credited_number VARCHAR(40),
    ADD COLUMN IF NOT EXISTS credited_issue_date DATE,
    ADD COLUMN IF NOT EXISTS return_id UUID REFERENCES order_returns(id);
CREATE INDEX IF NOT EXISTS idx_invoices_credited ON invoices (credited_invoice_id) WHERE credited_invoice_id IS NOT NULL;

-- A card refund on the terminal, once it paid a return back
ALTER TABLE terminal_transactions
    ADD COLUMN IF NOT EXISTS return_id UUID REFERENCES order_returns(id);

-- Points taken back or given back by a return: several per order, one of each
-- kind per return; an order's own award, redemption and full undoing stay once
ALTER TABLE loyalty_entries
    ADD COLUMN IF NOT EXISTS return_id UUID REFERENCES order_returns(id) ON DELETE SET NULL;
DROP INDEX IF EXISTS loyalty_entries_order_kind_idx;
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_entries_order_kind_idx
    ON loyalty_entries (order_id, kind)
    WHERE order_id IS NOT NULL AND return_id IS NULL AND kind IN ('earn', 'redeem', 'reverse', 'refund');
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_entries_return_kind_idx
    ON loyalty_entries (return_id, kind) WHERE return_id IS NOT NULL;
