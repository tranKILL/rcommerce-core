-- Preferred language of a customer (BCP 47 tag, e.g. "fr" or "nl-be"): the
-- language of the e-mails they receive. NULL = the store's default language.
ALTER TABLE customers ADD COLUMN IF NOT EXISTS locale VARCHAR(35);
