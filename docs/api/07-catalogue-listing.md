# Catalogue listing and search

`GET /api/v1/products` returns active products, newest first, with `meta.total`,
`meta.page`, `meta.per_page` and `meta.total_pages`.

| Parameter | Meaning |
|---|---|
| `category` | category slug |
| `q` | search (below) |
| `sort` | `newest` (default), `best_selling`, `price_asc`, `price_desc`, `name_asc`, `name_desc` |
| `min_price`, `max_price` | price range, excluding tax (inclusive) |
| `featured` | `true`: only products staff marked as featured |
| `on_sale` | `true`: only products with a retail offer: a reduced price or a promotion such as buy X get Y (promotions on the whole cart do not count) |
| `page`, `per_page` | 1-based page, 1 to 100 per page (default 20) |

An unknown `sort`, or `min_price` above `max_price`, is a `400` validation error.
`meta.total` counts with the same filters as the page.

`best_selling` orders by units sold over the last 90 days in confirmed orders
(confirmed, processing, shipped, delivered or completed; test and draft orders left
out), newest first among equals.

## Categories

`GET /api/v1/categories` gives each category its `product_count`: the active products
in it and in every category below it, each counted once. Storefronts can leave out
the categories at `0` from menus and sitemaps.

## Search

`q` is split into words (at most 8). Every word must appear in the title, the
description, the SKU or a variant SKU. Both the words and the product texts are
compared in a normalised form (SQL function `catalog_search_text`, migration 030):

- lower case, accents removed: `beton` finds "Béton";
- spaces removed: `25kg` finds "25 kg", and `ciment portland` still matches;
- a comma between digits read as a decimal point: `1.2mm` finds "1,2 mm";
- `%` and `_` are taken literally.

When `q` equals a product's SKU (ignoring case), that product comes first, before
the requested `sort`.

```bash
GET /api/v1/products?q=st-0005
GET /api/v1/products?q=ciment+25kg&sort=price_asc&min_price=5&max_price=20
GET /api/v1/products?sort=best_selling&per_page=12
GET /api/v1/products?on_sale=true
```
