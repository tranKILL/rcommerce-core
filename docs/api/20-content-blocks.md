# Content blocks

Short storefront texts staff edit: the home page banner, the home page sections,
the footer, the titles and descriptions search engines show. A block is a set of text
fields under a key, one block per language. A storefront shows its built-in texts
where a block, or a field, is missing.

## Reading (public)

- `GET /content/{key}?lang=fr`: the block in the language asked for, else in the
  store's default language, else the one shared by every language (`und`); `404`
  when none exists. The answer says which `locale` it came from.
- `GET /content?keys=home_hero,footer,site_meta,site_settings&lang=fr`: up to 10
  blocks in one call, each found the same way, as `{ "blocks": { "<key>": { key,
  locale, content, updated_at } } }`. Keys without a block are left out. Storefronts
  read their whole frame with it once per page, or keep it a minute.

A storefront should use a per-language block only when its `locale` is the page's
language: a French footer on a Dutch page is worse than the built-in one.

## Editing (staff with `settings`)

- `GET /admin/content/{key}`: `{ "languages": { "fr": { ... }, "nl": { ... } } }`.
- `PUT /admin/content/{key}` with the same shape replaces the languages given. Fields
  are trimmed; empty ones are dropped, and a language left with no field loses its
  block. Field names are lower-case words (`a-z`, digits, `_`), texts at most 2000
  characters.

## Blocks with their own rules

Other keys (`home_hero` with `title` and `text`, ...) take any field names.

### `site_settings`, shared by every language

Saved under the language `und` only (`400` under any other).

| Field | Rule |
|---|---|
| `home_sections` | The home page sections, top to bottom, comma-separated, each once: `hero`, `departments`, `best_sellers`, `promotions`, `featured`, `branches`, `news`, `wiki`. Product, news and guide sections take a count of 1 to 48: `hero, best_sellers:8, branches`. |
| `google_verification`, `bing_verification` | The verification code alone (letters, digits, `-`, `_`, at most 100), for the `google-site-verification` and `msvalidate.01` meta tags. |

### `footer`, per language

| Field | Rule |
|---|---|
| `about`, `contact` | Free text. |
| `column_1_title` … `column_4_title` | At most 60 characters. |
| `column_1_links` … `column_4_links`, `legal_links`, `social_links` | One link a line, `Label \| address`, at most 12 lines; labels 1 to 80 characters. |

An address is a page of the site (`/fr/depots`), an `https` address with a host,
`mailto:` or `tel:`. Scripts (`javascript:`), plain `http` and protocol-relative
addresses (`//host`) are refused.

### `site_meta`, per language

| Field | Rule |
|---|---|
| `title_suffix` | At most 60 characters; replaces the store name at the end of page titles. |
| `home_title` | At most 70 characters. |
| `home_description` | At most 200 characters. |
| `share_image` | An `https` address, or an uploaded `/media/` file: the image of shared pages that have none of their own. |

Any other field name in these three blocks is a `400`, so a typo shows at once.

Table: `content_blocks` (migration 037).
