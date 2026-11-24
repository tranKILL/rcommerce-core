-- Sales per product (the best-selling order of listings) read order lines by product
CREATE INDEX IF NOT EXISTS idx_order_items_product_id ON order_items(product_id);
