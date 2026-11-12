-- Turkish and Romanian get their PostgreSQL stemmers in catalogue search
-- (Polish has none built in and stays on 'simple').
CREATE OR REPLACE FUNCTION rc_search_config(locale TEXT) RETURNS regconfig
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE split_part(replace(lower(locale), '_', '-'), '-', 1)
        WHEN 'en' THEN 'english'::regconfig
        WHEN 'fr' THEN 'french'::regconfig
        WHEN 'nl' THEN 'dutch'::regconfig
        WHEN 'de' THEN 'german'::regconfig
        WHEN 'es' THEN 'spanish'::regconfig
        WHEN 'it' THEN 'italian'::regconfig
        WHEN 'pt' THEN 'portuguese'::regconfig
        WHEN 'tr' THEN 'turkish'::regconfig
        WHEN 'ro' THEN 'romanian'::regconfig
        ELSE 'simple'::regconfig
    END
$$;

-- Generated search vectors and the title index are only computed when a row
-- is written: rewrite the rows of the two languages
UPDATE product_translations SET title = title
WHERE split_part(replace(lower(locale), '_', '-'), '-', 1) IN ('tr', 'ro');
