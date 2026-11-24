# Product documents and the link checker

## Product documents

Each product can list up to 30 links to documents and pages about it:

| Kind | For |
|---|---|
| `technical_sheet` | the maker's technical sheet |
| `performance_declaration` | declaration of performance (construction products) |
| `safety_sheet` | safety data sheet |
| `manual` | instructions, installation guide |
| `maker_page` | the product on the maker's site |
| `video` | a video |
| `other` | anything else |

A link has an `http` or `https` address with a host (at most 2000 characters, no
quotes, spaces or angle brackets), an optional `locale` (the document's language;
none: any) and an optional `label` (120 characters).

- `GET /products/{id}/links?lang=nl` (public): `{ data: [{ id, kind, url, locale,
  label, position }] }`. Links in the language asked for come first, then links for
  any language, then the others. Links the checker found broken are left out.
- `GET /admin/products/{id}/links` (`products` read): `{ links: [...] }`.
- `PUT /admin/products/{id}/links` (`products` write) with `{ links: [{ kind, url,
  locale, label }] }` replaces them, in that order. `404` for an unknown product,
  `400` for a broken list. New addresses are checked on the checker's next run.

## The link checker

With `[link_check] enabled = true` the API checks every hour, one server at a time,
the outside addresses the store shows: product documents, links in article bodies,
and footer links. Each address is checked once a week, and every day while it fails.

| State | When |
|---|---|
| `ok` | a 2xx answer, after temporary redirects only |
| `moved` | a permanent redirect (301, 308) on the way; its target is in `final_url` |
| `broken` | 404 or 410; a host that cannot be reached (name, TLS, connection); an address that is not public; three server errors or timeouts in a row |
| `uncertain` | 403 or 429 (some sites refuse robots); a first or second server error or timeout |
| `pending` | not checked yet |

**Requests.**
- **Method:** HEAD, then a one-byte GET for sites that refuse HEAD.
- **Limits:** 10 seconds per request, 5 redirects at most, one request per host per second.
- **User agent:** a generic one.

**Public addresses only.**
- **Resolving:** each host is resolved first. Private, loopback, link-local,
  carrier-grade NAT, multicast and documentation addresses are refused, in IPv4 and
  IPv6, including IPv4 inside IPv6.
- **No second lookup:** the request goes to the address that was checked.
- **Redirects:** each redirect is checked the same way.

**Staff (`products`):**
- `GET /admin/links?state=broken|moved|uncertain|pending|ok`: `{ data: [{ url,
  state, status, error, final_url, consecutive_failures, failing_since,
  last_checked_at, used_in: [{ kind: product|article|footer, id, title }] }] }`,
  500 at most.
- `POST /admin/links/recheck` with `{ url }`: checks an address the store shows now,
  and answers its new record.
- `POST /admin/links/replace` with `{ old, new }`: replaces the address in product
  links, article bodies and footers, and answers `{ changed }`. The new address must
  be `https`. Each replacement is logged (`audit` target).

Tables: `product_links`, `link_checks` (migration 056).
