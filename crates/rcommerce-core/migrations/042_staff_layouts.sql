-- What each staff role sees, and each staff member's own dashboard.
--
-- staff_roles.menu: back-office menu entries shown to the role (ids chosen by
-- the app); staff_roles.widgets: dashboard widgets offered to the role, in
-- their default order. NULL means everything the role's permissions allow.
-- These only narrow what is shown: permissions remain the only access check.
-- staff_roles.max_discount_percent: the largest manual discount the role may
-- give at the till (NULL: none).
--
-- staff_dashboard_layouts: a staff member's widgets in order, with their size
-- and whether they are hidden. Without a row, the roles' default applies.

ALTER TABLE staff_roles
    ADD COLUMN IF NOT EXISTS menu TEXT[],
    ADD COLUMN IF NOT EXISTS widgets TEXT[],
    ADD COLUMN IF NOT EXISTS max_discount_percent NUMERIC(5, 2)
        CHECK (max_discount_percent >= 0 AND max_discount_percent <= 100);

CREATE TABLE IF NOT EXISTS staff_dashboard_layouts (
    customer_id UUID PRIMARY KEY REFERENCES customers(id) ON DELETE CASCADE,
    layout JSONB NOT NULL DEFAULT '[]',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
