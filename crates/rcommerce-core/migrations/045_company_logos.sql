-- A company's logo, printed beside its name on the documents addressed to it
-- (order summaries, invoices, credit notes). Each upload is a new, immutable
-- row, already shrunk and compressed; the company points to its current one
-- and an invoice to the one it was issued with, so issued documents never
-- change. Rows no document and no company points to are deleted.

CREATE TABLE IF NOT EXISTS company_logos (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    image BYTEA NOT NULL,
    content_type VARCHAR(20) NOT NULL CHECK (content_type IN ('image/png', 'image/jpeg')),
    width INTEGER NOT NULL CHECK (width > 0),
    height INTEGER NOT NULL CHECK (height > 0),
    uploaded_by UUID REFERENCES customers(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_company_logos_company ON company_logos (company_id);

ALTER TABLE companies
    ADD COLUMN IF NOT EXISTS logo_id UUID REFERENCES company_logos(id) ON DELETE SET NULL;
ALTER TABLE invoices
    ADD COLUMN IF NOT EXISTS buyer_logo_id UUID REFERENCES company_logos(id) ON DELETE SET NULL;
