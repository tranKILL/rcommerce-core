//! Rules of the content blocks a storefront reads by name: the site settings
//! (home page sections, search engine verification codes), the footer and the
//! site meta texts. Other keys keep the free rules of [`super::content_service`].

use crate::services::content_service::Fields;
use crate::{Error, Result};

/// Locale of the blocks shared by every language (BCP 47 "undetermined")
pub const ALL_LANGUAGES: &str = "und";

/// Home page sections, top to bottom, with an optional product count (`best_sellers:12`)
pub const SITE_SETTINGS: &str = "site_settings";
/// Footer texts and links, per language
pub const FOOTER: &str = "footer";
/// Title suffix, home title and description, share image, per language
pub const SITE_META: &str = "site_meta";

/// Sections a home page can show
pub const HOME_SECTIONS: [&str; 6] = [
    "hero",
    "departments",
    "best_sellers",
    "promotions",
    "featured",
    "branches",
];
/// Most products a home page section lists
const MAX_SECTION_PRODUCTS: u32 = 48;
/// Most links in one footer list
const MAX_LINKS: usize = 12;

/// Whether `key`'s block is shared by every language
pub fn shared(key: &str) -> bool {
    key == SITE_SETTINGS
}

/// Check the cleaned `fields` of block `key` in `locale`
pub fn check(key: &str, locale: &str, fields: &Fields) -> Result<()> {
    if shared(key) != (locale == ALL_LANGUAGES) {
        return Err(Error::validation(if shared(key) {
            format!("{key} is shared by every language: save it under \"{ALL_LANGUAGES}\"")
        } else {
            format!("\"{ALL_LANGUAGES}\" is only for blocks shared by every language")
        }));
    }
    match key {
        SITE_SETTINGS => check_settings(fields),
        FOOTER => check_footer(fields),
        SITE_META => check_meta(fields),
        _ => Ok(()),
    }
}

fn unknown(key: &str, name: &str) -> Error {
    Error::validation(format!("{key} has no field {name}"))
}

fn too_long(name: &str, text: &str, max: usize) -> Result<()> {
    if text.chars().count() > max {
        return Err(Error::validation(format!(
            "{name} is longer than {max} characters"
        )));
    }
    Ok(())
}

fn check_settings(fields: &Fields) -> Result<()> {
    for (name, text) in fields {
        match name.as_str() {
            "home_sections" => check_sections(text)?,
            "google_verification" | "bing_verification" => {
                let ok = text.len() <= 100
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                if !ok {
                    return Err(Error::validation(format!(
                        "{name} must be the code alone (letters, digits, - and _)"
                    )));
                }
            }
            _ => return Err(unknown(SITE_SETTINGS, name)),
        }
    }
    Ok(())
}

/// `hero, best_sellers:12, branches`: known sections, each once, counts 1 to 48
fn check_sections(text: &str) -> Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, count) = match item.split_once(':') {
            Some((name, count)) => (name.trim(), Some(count.trim())),
            None => (item, None),
        };
        if !HOME_SECTIONS.contains(&name) {
            return Err(Error::validation(format!(
                "unknown home section {name}; known: {}",
                HOME_SECTIONS.join(", ")
            )));
        }
        if seen.contains(&name) {
            return Err(Error::validation(format!(
                "home section {name} is listed twice"
            )));
        }
        seen.push(name);
        if let Some(count) = count {
            let ok = count
                .parse::<u32>()
                .is_ok_and(|n| (1..=MAX_SECTION_PRODUCTS).contains(&n));
            if !ok {
                return Err(Error::validation(format!(
                    "{name}: the count must be 1 to {MAX_SECTION_PRODUCTS}"
                )));
            }
        }
    }
    Ok(())
}

fn check_footer(fields: &Fields) -> Result<()> {
    for (name, text) in fields {
        let column = name
            .strip_prefix("column_")
            .and_then(|rest| rest.split_once('_'))
            .filter(|(n, _)| matches!(*n, "1" | "2" | "3" | "4"))
            .map(|(_, part)| part);
        match (name.as_str(), column) {
            ("about" | "contact", _) => {}
            (_, Some("title")) => too_long(name, text, 60)?,
            ("legal_links" | "social_links", _) | (_, Some("links")) => check_links(name, text)?,
            _ => return Err(unknown(FOOTER, name)),
        }
    }
    Ok(())
}

/// One link a line, `Label | address`
fn check_links(name: &str, text: &str) -> Result<()> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() > MAX_LINKS {
        return Err(Error::validation(format!(
            "{name} holds more than {MAX_LINKS} links"
        )));
    }
    for line in lines {
        let (label, address) = line.split_once('|').ok_or_else(|| {
            Error::validation(format!("{name}: write each link as \"Label | address\""))
        })?;
        let (label, address) = (label.trim(), address.trim());
        if label.is_empty() || label.chars().count() > 80 {
            return Err(Error::validation(format!(
                "{name}: each label has 1 to 80 characters"
            )));
        }
        if !link_address(address) {
            return Err(Error::validation(format!(
                "{name}: {address} is not an https address, a page of the site (/...), mailto: or tel:"
            )));
        }
    }
    Ok(())
}

/// A page of the site (`/fr/depots`), an https address with a host, `mailto:`
/// or `tel:`; never a script, nor a protocol-relative `//host`
pub fn link_address(address: &str) -> bool {
    // Phone numbers keep the spaces people write them with
    if let Some(rest) = address.strip_prefix("tel:") {
        return (1..=40).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || b"+-. ()".contains(&b));
    }
    let plain = address.len() <= 500
        && !address.contains(['"', '<', '>', '\\', ' ', '\t'])
        && !address.chars().any(char::is_control);
    if !plain {
        return false;
    }
    if let Some(path) = address.strip_prefix('/') {
        return !path.starts_with('/');
    }
    if let Some(rest) = address.strip_prefix("mailto:") {
        return rest.contains('@');
    }
    address
        .strip_prefix("https://")
        .is_some_and(|rest| !rest.starts_with('/'))
        && url::Url::parse(address).is_ok_and(|u| u.host_str().is_some())
}

fn check_meta(fields: &Fields) -> Result<()> {
    for (name, text) in fields {
        match name.as_str() {
            "title_suffix" => too_long(name, text, 60)?,
            "home_title" => too_long(name, text, 70)?,
            "home_description" => too_long(name, text, 200)?,
            "share_image" => {
                if !crate::models::location::image_address(text) {
                    return Err(Error::validation(
                        "share_image must be an https address or an uploaded /media/ file",
                    ));
                }
            }
            _ => return Err(unknown(SITE_META, name)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn site_settings_are_shared_by_every_language() {
        let ok = fields(&[("home_sections", "hero, best_sellers:12, branches")]);
        assert!(check(SITE_SETTINGS, ALL_LANGUAGES, &ok).is_ok());
        assert!(check(SITE_SETTINGS, "fr", &ok).is_err());
        assert!(check(FOOTER, ALL_LANGUAGES, &fields(&[])).is_err());
        assert!(check("home_hero", "fr", &fields(&[("title", "x")])).is_ok());
    }

    #[test]
    fn home_sections_are_known_once_with_sane_counts() {
        for bad in [
            "hero, hero",
            "slider",
            "best_sellers:0",
            "best_sellers:49",
            "featured:x",
        ] {
            let f = fields(&[("home_sections", bad)]);
            assert!(check(SITE_SETTINGS, ALL_LANGUAGES, &f).is_err(), "{bad}");
        }
        let f = fields(&[("google_verification", "abc\"><script>")]);
        assert!(check(SITE_SETTINGS, ALL_LANGUAGES, &f).is_err());
        let f = fields(&[("bing_verification", "0A1B2C_d-e")]);
        assert!(check(SITE_SETTINGS, ALL_LANGUAGES, &f).is_ok());
        let f = fields(&[("theme", "dark")]);
        assert!(check(SITE_SETTINGS, ALL_LANGUAGES, &f).is_err());
    }

    #[test]
    fn footer_links_are_safe_addresses() {
        let ok = fields(&[
            ("column_1_title", "Services"),
            (
                "column_1_links",
                "Branches | /fr/depots\nContact | mailto:hello@example.com\nCall | tel:+32 2 000 00 00",
            ),
            ("legal_links", "Terms | https://example.com/terms"),
        ]);
        assert!(check(FOOTER, "fr", &ok).is_ok());
        for bad in [
            "x | javascript:alert(1)",
            "x | //evil.example",
            "x | http://example.com",
            "no separator",
            "x | https:///nohost",
            " | /fr",
        ] {
            let f = fields(&[("legal_links", bad)]);
            assert!(check(FOOTER, "fr", &f).is_err(), "{bad}");
        }
        assert!(check(FOOTER, "fr", &fields(&[("column_5_title", "x")])).is_err());
        let many = (0..13)
            .map(|i| format!("L{i} | /p{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(check(FOOTER, "fr", &fields(&[("social_links", &many)])).is_err());
    }

    #[test]
    fn meta_texts_have_search_engine_lengths() {
        let ok = fields(&[
            ("title_suffix", "The Store"),
            ("home_description", "Building materials"),
            ("share_image", "https://cdn.example.com/share.jpg"),
        ]);
        assert!(check(SITE_META, "en", &ok).is_ok());
        let long = "x".repeat(71);
        assert!(check(SITE_META, "en", &fields(&[("home_title", &long)])).is_err());
        let f = fields(&[("share_image", "javascript:alert(1)")]);
        assert!(check(SITE_META, "en", &f).is_err());
    }
}
