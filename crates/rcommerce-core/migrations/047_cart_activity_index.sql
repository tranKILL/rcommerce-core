-- The cleanup of inactive carts (`[carts] delete_inactive_after_months`)
-- finds them by their last change
CREATE INDEX IF NOT EXISTS idx_carts_updated_at ON carts (updated_at);
