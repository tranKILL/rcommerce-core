# News, guides and pictures

News items and building guides (a "wiki"), with categories and the products they are
about, and the pictures staff upload for the site. Staff need the `content` permission
(`read` for the `GET` routes, `write` for the others); it covers the routes below only,
so a writer can publish news without touching the store's settings.

## Articles

An article has:
- a `kind`: `news` or `wiki`;
- a `slug`, shared by every language: lower-case letters and digits joined by hyphens,
  unique across articles;
- an optional `category_id`, which must be a category of the same kind;
- an optional `cover_image_url`: an `https` address or an uploaded `/media/` file;
- `is_published`, `published_at` (set to now when published without one), and for
  news an optional `expires_at`, which must come after `published_at`;
- `texts`: a title (required, at most 200 characters), summary (500), body, search title
  (70) and search description (200) per language;
- `product_ids`: up to 50 products, in order.

Shoppers see an article when it is published, from `published_at` on, until
`expires_at` when there is one, newest first.

### Cleaned bodies

Bodies come from a rich text editor. On save the API keeps only headings 2 to 4,
paragraphs, line breaks, rules, emphasis, lists, quotes, figures, tables, links and
pictures. It drops scripts, styles, event handlers and every other tag or attribute.
Links must be site pages (`/fr/...`), `https`, `mailto:` or `tel:` addresses, and
pictures `https` or `/media/` files; others lose their address. A body is at most
300,000 characters as sent and 200,000 once cleaned.

### Public

- `GET /articles?kind=news|wiki&category=<slug>&lang=fr&page=1&per_page=12` (at most
  50): `{ data: [summary], meta: { total, page, per_page, total_pages } }`. A summary is
  `id, kind, slug, locale, title, summary, cover_image_url, published_at, expires_at,
  category_slug, category_name`, in the language asked for, else the store's, else
  any; `locale` says which.
- `GET /articles/{slug}?lang=fr`: a summary plus `body_html`, `seo_title`,
  `seo_description`, `locales` (the languages it is written in) and `products` (`id,
  slug, title, image` of the active ones). `404` for drafts and ended news.
- `GET /products/{id}/articles?lang=fr`: `{ data: [summary] }`, at most 20.
- `GET /article-categories?kind=wiki&lang=fr`: `{ data: [{ id, kind, slug, position,
  name }] }`.

### Staff

- `GET /admin/articles?kind=&q=&lang=&page=`: drafts included; `q` searches the titles.
  Lists take pages 1 to 10,000.
- `POST /admin/articles` (`201`), `GET`, `PUT` and `DELETE /admin/articles/{id}`. The
  answer has every language's texts and the product ids. Changes to articles,
  categories and pictures are logged (`audit` target) with the staff member.
- `GET` and `POST /admin/article-categories`; `PUT` and `DELETE
  /admin/article-categories/{id}` with `{ kind, slug, position, names: { fr: "…" } }`.
  A deleted category leaves its articles uncategorised.

## Pictures

- `POST /admin/media?name=wall` with the picture as the body (PNG, JPEG or WebP, at
  most 8 MB and 6000 pixels a side; the API decodes two at a time): `201` with `{ id,
  url, content_type, width, height, size, name, created_at }`. The picture is decoded and encoded again, which
  drops its metadata (camera, place) and anything that is not a picture: JPEG
  (quality 85) when opaque, PNG when some pixel is transparent, shrunk to fit 2400
  pixels. Anything else is a `400`.
- `GET /admin/media?page=`: the newest 48 per page. `DELETE /admin/media/{id}`.
- `GET /media/{id}.jpg` (or `.png`), public: the picture, `Cache-Control: public,
  max-age=2592000` (a new upload is a new id; a deleted picture leaves caches within 30
  days), `X-Content-Type-Options: nosniff`.

Articles, shops and the site meta refer to pictures as `/media/{id}.jpg`. A storefront
serves that path from the API on its own address, so pages keep one origin.

Tables: `articles`, `article_translations`, `article_categories`,
`article_category_translations`, `product_articles`, `media_files` (migration 055).
