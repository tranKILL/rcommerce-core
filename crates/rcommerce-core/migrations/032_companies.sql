-- Business customers: customer groups (price lists can target one), companies
-- with a checked VAT number, and the customers who buy for them.

CREATE TABLE IF NOT EXISTS customer_groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code VARCHAR(50) NOT NULL UNIQUE,
    name VARCHAR(100) NOT NULL,
    -- Members see prices excluding VAT first
    prices_exclude_tax BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Every new company joins this group
INSERT INTO customer_groups (code, name, prices_exclude_tax)
VALUES ('trade', 'Trade', TRUE)
ON CONFLICT (code) DO NOTHING;

-- A customer outside any company can still be put in a group by staff
ALTER TABLE customers
    ADD COLUMN IF NOT EXISTS customer_group_id UUID REFERENCES customer_groups(id) ON DELETE SET NULL;

CREATE TABLE IF NOT EXISTS companies (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name VARCHAR(255) NOT NULL,
    -- Country prefix and number, e.g. BE0123456749
    vat_number VARCHAR(20) NOT NULL UNIQUE,
    country CHAR(2) NOT NULL,
    company_number VARCHAR(20),
    -- Registered address as the VAT register gives it
    address TEXT,
    customer_group_id UUID REFERENCES customer_groups(id) ON DELETE SET NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
    -- When the VAT register confirmed the number (NULL = format check only)
    vat_checked_at TIMESTAMPTZ,
    -- Peppol participant id (e.g. 0208:0123456749) when the company receives e-invoices
    peppol_id VARCHAR(64),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_companies_created ON companies (created_at DESC);

-- A customer buys for at most one company
CREATE TABLE IF NOT EXISTS company_members (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    customer_id UUID NOT NULL UNIQUE REFERENCES customers(id) ON DELETE CASCADE,
    role VARCHAR(20) NOT NULL CHECK (role IN ('owner', 'buyer')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (company_id, customer_id)
);
