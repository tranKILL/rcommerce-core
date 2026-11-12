-- Short texts staff edit for the storefront (the home page banner...), one
-- per key and language
CREATE TABLE IF NOT EXISTS content_blocks (
    key VARCHAR(50) NOT NULL,
    locale VARCHAR(16) NOT NULL,
    content JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by UUID,
    PRIMARY KEY (key, locale)
);
