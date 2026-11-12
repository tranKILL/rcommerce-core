//! Translated e-mail texts.
//!
//! Each language has one message catalogue: a TOML file with a section per
//! e-mail (`[order_ready_for_pickup]`) and a `[common]` section for texts every
//! e-mail shares (labels, footer, date and money formats). A template refers to
//! a text as `{{ t.key }}`, looked up in its own section, then in `[common]`.
//! Texts may hold the template's own `{{ variable }}` placeholders.
//!
//! English, French, Dutch, German, Italian, Spanish, Portuguese, Turkish,
//! Romanian and Polish are built in. `notifications.locales_dir` adds a
//! language (`de.toml`) or replaces some texts of a built-in one without a
//! rebuild. A text missing from a language falls back to English.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{OnceLock, RwLock};

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use crate::{Error, Result};

/// The language every catalogue falls back to; its catalogue is complete
pub const BASE_LOCALE: &str = "en";

const BUILT_IN: &[(&str, &str)] = &[
    ("en", include_str!("locales/en.toml")),
    ("fr", include_str!("locales/fr.toml")),
    ("nl", include_str!("locales/nl.toml")),
    ("de", include_str!("locales/de.toml")),
    ("it", include_str!("locales/it.toml")),
    ("es", include_str!("locales/es.toml")),
    ("pt", include_str!("locales/pt.toml")),
    ("tr", include_str!("locales/tr.toml")),
    ("ro", include_str!("locales/ro.toml")),
    ("pl", include_str!("locales/pl.toml")),
];

type Catalogue = toml::Table;

fn catalogues() -> &'static RwLock<HashMap<String, Catalogue>> {
    static CATALOGUES: OnceLock<RwLock<HashMap<String, Catalogue>>> = OnceLock::new();
    CATALOGUES.get_or_init(|| {
        let built_in = BUILT_IN
            .iter()
            .map(|(code, source)| {
                let table = source
                    .parse::<Catalogue>()
                    .unwrap_or_else(|e| panic!("built-in e-mail catalogue {code}: {e}"));
                (code.to_string(), table)
            })
            .collect();
        RwLock::new(built_in)
    })
}

/// Lower-case a locale tag and use `-` as separator (`fr_BE` -> `fr-be`)
fn normalize(locale: &str) -> String {
    locale.trim().replace('_', "-").to_ascii_lowercase()
}

/// Add every `<locale>.toml` catalogue found in `dir`. A new language is added
/// as is; for a known one, each text given replaces the built-in text. Returns
/// the languages read.
pub fn load_locales_dir(dir: &Path) -> Result<Vec<String>> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Error::config(format!("e-mail locales dir {}: {e}", dir.display())))?;
    let mut loaded = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(code) = path.file_stem().and_then(|s| s.to_str()).map(normalize) else {
            continue;
        };
        let source = std::fs::read_to_string(&path)
            .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
        add_catalogue(&code, &source)
            .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
        loaded.push(code);
    }
    loaded.sort();
    Ok(loaded)
}

/// Add or extend the catalogue of `locale` from TOML source (texts given
/// replace existing ones, section by section)
pub fn add_catalogue(locale: &str, source: &str) -> Result<()> {
    let table = source
        .parse::<Catalogue>()
        .map_err(|e| Error::validation(format!("invalid catalogue: {e}")))?;
    let mut all = catalogues().write().unwrap_or_else(|e| e.into_inner());
    let current = all.entry(normalize(locale)).or_default();
    for (section, texts) in table {
        match (current.get_mut(&section), texts) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(new)) => existing.extend(new),
            (_, texts) => {
                current.insert(section, texts);
            }
        }
    }
    Ok(())
}

/// The languages e-mails can be written in, sorted
pub fn available_locales() -> Vec<String> {
    let all = catalogues().read().unwrap_or_else(|e| e.into_inner());
    let mut codes: Vec<String> = all.keys().cloned().collect();
    codes.sort();
    codes
}

/// The catalogue `locale` resolves to: the tag itself (`fr-be`), else its language (`fr`)
fn resolve(locale: &str) -> Option<String> {
    let all = catalogues().read().unwrap_or_else(|e| e.into_inner());
    let tag = normalize(locale);
    if all.contains_key(&tag) {
        return Some(tag);
    }
    let language = tag.split('-').next().unwrap_or_default();
    all.contains_key(language).then(|| language.to_string())
}

/// The e-mail language: the first candidate that has a catalogue, English when none has
pub fn pick_locale<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> String {
    candidates
        .into_iter()
        .flatten()
        .find_map(resolve)
        .unwrap_or_else(|| BASE_LOCALE.to_string())
}

/// The first language listed in an `Accept-Language` header that has a catalogue
pub fn from_accept_language(header: &str) -> Option<String> {
    let mut tags: Vec<(f32, &str)> = header
        .split(',')
        .filter_map(|part| {
            let mut pieces = part.split(';');
            let tag = pieces.next()?.trim();
            let quality = pieces
                .find_map(|p| p.trim().strip_prefix("q="))
                .and_then(|q| q.parse::<f32>().ok())
                .unwrap_or(1.0);
            (!tag.is_empty() && tag != "*").then_some((quality, tag))
        })
        .collect();
    tags.sort_by(|a, b| b.0.total_cmp(&a.0));
    tags.into_iter().find_map(|(_, tag)| resolve(tag))
}

fn lookup(locale: &str, section: &str, key: &str) -> Option<toml::Value> {
    // Resolved before taking the lock: a second read lock while a writer
    // waits would deadlock
    let code = resolve(locale);
    let all = catalogues().read().unwrap_or_else(|e| e.into_inner());
    let find = |code: &str| {
        let catalogue = all.get(code)?;
        [section, "common"]
            .iter()
            .find_map(|s| catalogue.get(*s)?.get(key).cloned())
    };
    code.and_then(|code| find(&code))
        .or_else(|| find(BASE_LOCALE))
}

/// One text in `locale`: `key` from `section`, else from `[common]`, else the
/// English text; the key itself when no catalogue has it
pub fn text(locale: &str, section: &str, key: &str) -> String {
    match lookup(locale, section, key) {
        Some(toml::Value::String(s)) => s,
        _ => key.to_string(),
    }
}

/// Replace every `{{ t.key }}` in `template` with its text in `locale`
pub fn translate(template: &str, locale: &str, section: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{ t.") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        match after.find(" }}") {
            Some(end) => {
                out.push_str(&text(locale, section, after[..end].trim()));
                rest = &after[end + 3..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The symbol written for a currency code (the code itself when unknown);
/// dollars other than the US dollar keep a prefix so they stay distinct
pub fn currency_symbol(code: &str) -> &str {
    match code {
        "USD" => "$",
        "AUD" => "A$",
        "CAD" => "C$",
        "NZD" => "NZ$",
        "SGD" => "S$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" | "CNY" => "¥",
        "HKD" => "HK$",
        "CHF" => "CHF",
        other => other,
    }
}

/// An amount as written in `locale`, with two decimals and the currency placed
/// by `common.money_format` (`€1,234.50`, `1 234,50 €`, `€ 1.234,50`)
pub fn money(locale: &str, amount: Decimal, currency: &str) -> String {
    let rounded = amount.round_dp(2);
    let negative = rounded.is_sign_negative() && !rounded.is_zero();
    let plain = format!("{:.2}", rounded.abs());
    let (whole, cents) = plain.split_once('.').unwrap_or((&plain, "00"));
    let thousands = text(locale, "common", "thousands_separator");
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push_str(&thousands);
        }
        grouped.push(digit);
    }
    let number = format!(
        "{grouped}{}{cents}",
        text(locale, "common", "decimal_separator")
    );
    let written = text(locale, "common", "money_format")
        .replace("{symbol}", currency_symbol(currency))
        .replace("{amount}", &number);
    if negative {
        format!("-{written}")
    } else {
        written
    }
}

/// A date as written in `locale` (`common.date_format` with `{day}`, `{month}`
/// from `common.months`, and `{year}`)
pub fn date(locale: &str, day: NaiveDate) -> String {
    let month = match lookup(locale, "common", "months") {
        Some(toml::Value::Array(names)) => names
            .get(day.month0() as usize)
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    }
    .unwrap_or_else(|| day.month().to_string());
    text(locale, "common", "date_format")
        .replace("{day}", &day.day().to_string())
        .replace("{month}", &month)
        .replace("{year}", &day.year().to_string())
}

/// A duration in whole hours as written in `locale` ("24 hours", "1 heure")
pub fn hours(locale: &str, count: u32) -> String {
    let key = if count == 1 { "one_hour" } else { "hours" };
    text(locale, "common", key).replace("{n}", &count.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn picks_the_first_language_with_a_catalogue() {
        assert_eq!(pick_locale([None, Some("fr-BE"), Some("nl")]), "fr");
        assert_eq!(pick_locale([Some("xx"), Some("nl_BE")]), "nl");
        assert_eq!(pick_locale([Some("xx"), None]), "en");
        assert_eq!(
            from_accept_language("ja;q=0.9, nl-BE;q=0.8, en;q=0.5").as_deref(),
            Some("nl")
        );
        assert_eq!(pick_locale([Some("pl-PL")]), "pl");
    }

    /// The `{{ name }}` and `{name}` placeholders of a text, sorted
    fn placeholders(text: &str) -> Vec<String> {
        let mut found: Vec<String> = text
            .split('{')
            .skip(1)
            .filter_map(|part| part.split('}').next())
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn every_built_in_language_has_every_text_with_its_placeholders() {
        let base: toml::Table = BUILT_IN[0].1.parse().unwrap();
        for (code, source) in &BUILT_IN[1..] {
            let catalogue: toml::Table = source.parse().unwrap();
            for (section, texts) in &base {
                for (key, english) in texts.as_table().unwrap() {
                    let found = catalogue.get(section).and_then(|s| s.get(key));
                    let found =
                        found.unwrap_or_else(|| panic!("{code}: [{section}] {key} missing"));
                    match (english, found) {
                        (toml::Value::String(en), toml::Value::String(other)) => {
                            if !(section == "common" && key == "footer_note") {
                                assert_eq!(
                                    placeholders(en),
                                    placeholders(other),
                                    "{code}: [{section}] {key}"
                                );
                            }
                        }
                        (toml::Value::Array(en), toml::Value::Array(other)) => {
                            assert_eq!(en.len(), other.len(), "{code}: [{section}] {key}")
                        }
                        _ => panic!("{code}: [{section}] {key} has another type"),
                    }
                }
            }
        }
    }

    #[test]
    fn translates_keys_with_fallback_to_common_and_english() {
        let html = "<h1>{{ t.heading }}</h1><td>{{ t.order_number }}</td>{{ name }}";
        let fr = translate(html, "fr", "order_ready_for_pickup");
        assert!(fr.contains("Votre commande est prête"), "{fr}");
        assert!(fr.contains("Numéro de commande"), "{fr}");
        assert!(fr.ends_with("{{ name }}"));
        assert_eq!(
            text("fr", "order_ready_for_pickup", "no_such_key"),
            "no_such_key"
        );
    }

    #[test]
    fn writes_money_and_dates_per_language() {
        assert_eq!(money("en", dec!(1234.5), "EUR"), "€1,234.50");
        // Non-breaking spaces keep an amount on one line
        assert_eq!(money("fr", dec!(1234.5), "EUR"), "1\u{a0}234,50\u{a0}€");
        assert_eq!(money("nl", dec!(-7), "EUR"), "-€\u{a0}7,00");
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        assert_eq!(date("en", day), "November 9, 2026");
        assert_eq!(date("fr", day), "9 novembre 2026");
        assert_eq!(hours("fr", 1), "1 heure");
        assert_eq!(hours("nl", 24), "24 uur");
        assert_eq!(money("de", dec!(1234.5), "EUR"), "1.234,50\u{a0}€");
        assert_eq!(money("pl", dec!(1234.5), "EUR"), "1\u{a0}234,50\u{a0}€");
        assert_eq!(date("de", day), "9. November 2026");
        assert_eq!(date("es", day), "9 de noviembre de 2026");
        assert_eq!(date("pl", day), "9 listopada 2026");
        assert_eq!(
            text("tr", "order_document", "title_summary"),
            "Sipariş özeti"
        );
    }

    #[test]
    fn a_template_loads_in_the_customer_language() {
        use crate::notification::templates::{NotificationTemplate, TemplateVariables};
        let template =
            NotificationTemplate::load_localized("order_ready_for_pickup_html", "fr-BE").unwrap();
        assert_eq!(template.locale, "fr");
        let mut vars = TemplateVariables::new();
        vars.insert("order_number", "ORD-7");
        vars.insert("customer_name", "Anne");
        vars.insert("location_name", "Store North");
        assert_eq!(
            template.render_subject(&vars).unwrap(),
            "Votre commande ORD-7 est prête à être retirée"
        );
        let html = template.render_html(&vars).unwrap().unwrap();
        assert!(html.contains("<html lang=\"fr\">"));
        assert!(html.contains("Des questions"));
        assert!(html.contains("la commande <strong>ORD-7</strong> vous attend"));
        assert!(!html.contains("{{") && !html.contains("Payments are in USD"));
        assert!(template
            .render(&vars)
            .unwrap()
            .contains("retirée à Store North"));
        // A language without a catalogue gets the English texts
        let en = NotificationTemplate::load_localized("order_ready_for_pickup_html", "xx").unwrap();
        assert!(en
            .render_subject(&vars)
            .unwrap()
            .ends_with("is ready for pickup"));
    }

    #[test]
    fn a_catalogue_adds_a_language_or_replaces_texts() {
        // A private-use tag, so tests running alongside never see it
        add_catalogue("qx", "[common]\nquestions = \"Qx?\"\n").unwrap();
        assert_eq!(pick_locale([Some("qx-AT")]), "qx");
        assert_eq!(text("qx", "welcome", "questions"), "Qx?");
        // Texts the new language lacks come from English
        assert_eq!(
            text("qx", "common", "subtotal"),
            text("en", "common", "subtotal")
        );
        assert!(add_catalogue("qx", "not = [valid").is_err());
        // A file replaces only the texts it gives (a private-use tag again)
        add_catalogue("qy", "[order_shipped]\ncarrier = \"Qy\"\n").unwrap();
        add_catalogue("qy", "[order_shipped]\nin_transit = \"Qz\"\n").unwrap();
        assert_eq!(text("qy", "order_shipped", "carrier"), "Qy");
        assert_eq!(text("qy", "order_shipped", "in_transit"), "Qz");
    }
}
