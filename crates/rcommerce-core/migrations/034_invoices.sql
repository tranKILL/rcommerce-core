-- Legal invoices issued from orders: numbered without gaps per prefix and
-- year, immutable snapshots of seller, buyer, lines and VAT per rate, and the
-- state of their Peppol delivery.

CREATE TABLE IF NOT EXISTS invoice_counters (
    prefix VARCHAR(20) NOT NULL,
    year INTEGER NOT NULL,
    last_number BIGINT NOT NULL,
    PRIMARY KEY (prefix, year)
);

CREATE TABLE IF NOT EXISTS invoices (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    number VARCHAR(40) NOT NULL UNIQUE,
    kind VARCHAR(12) NOT NULL DEFAULT 'invoice' CHECK (kind IN ('invoice', 'credit_note')),
    order_id UUID NOT NULL REFERENCES orders(id),
    order_number VARCHAR(50) NOT NULL,
    customer_id UUID REFERENCES customers(id) ON DELETE SET NULL,
    company_id UUID REFERENCES companies(id) ON DELETE SET NULL,
    issue_date DATE NOT NULL,
    due_date DATE NOT NULL,
    currency VARCHAR(3) NOT NULL,
    seller JSONB NOT NULL,
    buyer JSONB NOT NULL,
    lines JSONB NOT NULL,
    vat JSONB NOT NULL,
    subtotal NUMERIC(19, 2) NOT NULL,
    tax_total NUMERIC(19, 2) NOT NULL,
    total NUMERIC(19, 2) NOT NULL,
    paid_amount NUMERIC(19, 2) NOT NULL DEFAULT 0,
    -- Belgian structured communication (+++123/4567/89012+++)
    payment_reference VARCHAR(24) NOT NULL,
    locale VARCHAR(35) NOT NULL,
    -- pending, sent, delivered, failed, not_on_peppol (NULL: not for Peppol)
    peppol_status VARCHAR(16),
    peppol_message_id VARCHAR(100),
    peppol_recipient VARCHAR(100),
    peppol_error TEXT,
    peppol_updated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- One invoice per order (credit notes come on top)
CREATE UNIQUE INDEX IF NOT EXISTS idx_invoices_one_per_order ON invoices (order_id) WHERE kind = 'invoice';
CREATE INDEX IF NOT EXISTS idx_invoices_customer ON invoices (customer_id);
CREATE INDEX IF NOT EXISTS idx_invoices_company ON invoices (company_id);
CREATE INDEX IF NOT EXISTS idx_invoices_peppol ON invoices (peppol_status) WHERE peppol_status IN ('pending', 'sent');
