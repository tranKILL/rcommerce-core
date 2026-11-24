-- News and building guides (wiki), with their categories and the products they
-- are about, and the pictures staff upload for them (and for shops and pages)

-- Uploaded pictures, re-encoded on upload; served at /media/<id>.<ext>
CREATE TABLE IF NOT EXISTS media_files (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    content_type VARCHAR(20) NOT NULL CHECK (content_type IN ('image/jpeg', 'image/png')),
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    size INTEGER NOT NULL,
    image BYTEA NOT NULL,
    -- the uploader's own name for it, to find it again
    name VARCHAR(200),
    created_by UUID REFERENCES customers(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_media_files_created ON media_files (created_at DESC);

CREATE TABLE IF NOT EXISTS article_categories (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind VARCHAR(10) NOT NULL CHECK (kind IN ('news', 'wiki')),
    slug VARCHAR(100) NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (kind, slug)
);

CREATE TABLE IF NOT EXISTS article_category_translations (
    category_id UUID NOT NULL REFERENCES article_categories(id) ON DELETE CASCADE,
    locale VARCHAR(16) NOT NULL,
    name VARCHAR(100) NOT NULL,
    PRIMARY KEY (category_id, locale)
);

CREATE TABLE IF NOT EXISTS articles (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind VARCHAR(10) NOT NULL CHECK (kind IN ('news', 'wiki')),
    category_id UUID REFERENCES article_categories(id) ON DELETE SET NULL,
    slug VARCHAR(150) NOT NULL UNIQUE,
    cover_image_url TEXT,
    is_published BOOLEAN NOT NULL DEFAULT false,
    -- shown from then on (newest first); NULL while a draft
    published_at TIMESTAMPTZ,
    -- news: gone from the lists after this; NULL: stays
    expires_at TIMESTAMPTZ,
    author_id UUID REFERENCES customers(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (expires_at IS NULL OR published_at IS NULL OR expires_at > published_at)
);
CREATE INDEX IF NOT EXISTS idx_articles_listing
    ON articles (kind, published_at DESC) WHERE is_published;

CREATE TABLE IF NOT EXISTS article_translations (
    article_id UUID NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    locale VARCHAR(16) NOT NULL,
    title VARCHAR(200) NOT NULL,
    summary VARCHAR(500),
    -- cleaned on save (allowed tags and attributes only)
    body_html TEXT NOT NULL DEFAULT '',
    seo_title VARCHAR(70),
    seo_description VARCHAR(200),
    PRIMARY KEY (article_id, locale)
);

CREATE TABLE IF NOT EXISTS product_articles (
    product_id UUID NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    article_id UUID NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (product_id, article_id)
);
CREATE INDEX IF NOT EXISTS idx_product_articles_article ON product_articles (article_id);
