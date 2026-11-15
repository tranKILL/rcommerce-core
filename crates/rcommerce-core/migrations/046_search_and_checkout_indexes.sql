-- Indexes the load test showed missing (100 customers, 8 staff, 3 cashiers on
-- three years of orders):
--
-- * Checkout looks up the order already created for a cart (idempotent
--   retries) by `metadata->>'cart_id'`: a full scan of the orders table at
--   every checkout without this index.
-- * The substring search (catalogue search when full text finds nothing:
--   "25kg" in "25 kg", "1,2" in "1.2 mm") compares catalog_search_text() of
--   titles and SKUs with LIKE '%word%': trigram indexes on those expressions
--   let it read only the candidate rows.

CREATE INDEX IF NOT EXISTS idx_orders_cart_id
    ON orders ((metadata->>'cart_id'), created_at DESC);

CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE INDEX IF NOT EXISTS idx_products_search_title_trgm
    ON products USING gin (catalog_search_text(title) gin_trgm_ops);
CREATE INDEX IF NOT EXISTS idx_products_search_sku_trgm
    ON products USING gin (catalog_search_text(sku) gin_trgm_ops);
CREATE INDEX IF NOT EXISTS idx_product_variants_search_sku_trgm
    ON product_variants USING gin (catalog_search_text(sku) gin_trgm_ops);
CREATE INDEX IF NOT EXISTS idx_product_translations_search_title_trgm
    ON product_translations USING gin (catalog_search_text(title) gin_trgm_ops);
