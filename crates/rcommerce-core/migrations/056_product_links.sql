-- Documents and pages about a product (technical sheets, declarations of
-- performance, makers' pages...), and the result of checking every outside
-- link the store shows (product documents, article bodies, the footer)

CREATE TABLE IF NOT EXISTS product_links (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id UUID NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    kind VARCHAR(30) NOT NULL CHECK (kind IN ('technical_sheet', 'performance_declaration',
        'safety_sheet', 'manual', 'maker_page', 'video', 'other')),
    url TEXT NOT NULL,
    -- the document's language; NULL: any
    locale VARCHAR(16),
    label VARCHAR(120),
    position INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_product_links_product ON product_links (product_id, position);
CREATE INDEX IF NOT EXISTS idx_product_links_url ON product_links (url);

CREATE TABLE IF NOT EXISTS link_checks (
    url TEXT PRIMARY KEY,
    -- pending (never checked), ok, broken, moved (permanent redirect), uncertain
    state VARCHAR(12) NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'ok', 'broken', 'moved', 'uncertain')),
    status INTEGER,
    error TEXT,
    -- where a permanent redirect leads: the suggested replacement
    final_url TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    failing_since TIMESTAMPTZ,
    ok_at TIMESTAMPTZ,
    last_checked_at TIMESTAMPTZ,
    next_check_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_link_checks_due ON link_checks (next_check_at);
CREATE INDEX IF NOT EXISTS idx_link_checks_state ON link_checks (state) WHERE state <> 'ok';
