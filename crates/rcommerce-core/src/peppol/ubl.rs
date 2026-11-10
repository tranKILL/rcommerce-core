//! UBL 2.1 invoices in the Peppol BIS Billing 3.0 profile (EN 16931).

use base64::Engine;
use rust_decimal::Decimal;

use crate::invoicing::{Invoice, InvoiceLine, Party, VatSubtotal};

const CUSTOMIZATION: &str =
    "urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0";
const PROFILE: &str = "urn:fdc:peppol.eu:2017:poacc:billing:01:1.0";

/// Text made safe for XML content and attributes
fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn amount(value: Decimal) -> String {
    format!("{:.2}", value.round_dp(2))
}

fn percent(rate: Decimal) -> String {
    rate.normalize().to_string()
}

/// The VAT category of a rate: standard (`S`), intra-EU reverse charge (`AE`)
/// for a zero rate to a business in another country, else zero rated (`Z`)
fn category(rate: Decimal, invoice: &Invoice) -> &'static str {
    if rate > Decimal::ZERO {
        "S"
    } else if invoice.buyer.vat_number.is_some() && invoice.buyer.country != invoice.seller.country
    {
        "AE"
    } else {
        "Z"
    }
}

/// `<cbc:EndpointID schemeID="0208">0123456749</cbc:EndpointID>` from `0208:0123456749`
fn endpoint(participant: &str) -> String {
    let (scheme, id) = participant.split_once(':').unwrap_or(("0208", participant));
    format!(
        "<cbc:EndpointID schemeID=\"{}\">{}</cbc:EndpointID>",
        esc(scheme),
        esc(id)
    )
}

/// A party's postal address (empty parts left out; the country is required)
fn postal_address(who: &Party) -> String {
    let mut xml = String::from("<cac:PostalAddress>");
    for (tag, value) in [
        ("StreetName", &who.street),
        ("CityName", &who.city),
        ("PostalZone", &who.postcode),
    ] {
        if !value.is_empty() {
            xml.push_str(&format!("<cbc:{tag}>{}</cbc:{tag}>", esc(value)));
        }
    }
    xml.push_str(&format!(
        "<cac:Country><cbc:IdentificationCode>{}</cbc:IdentificationCode></cac:Country></cac:PostalAddress>",
        esc(&who.country)
    ));
    xml
}

fn party(tag: &str, who: &Party, participant: &str) -> String {
    let mut xml = format!("<cac:{tag}><cac:Party>{}", endpoint(participant));
    xml.push_str(&format!(
        "<cac:PartyName><cbc:Name>{}</cbc:Name></cac:PartyName>",
        esc(&who.name)
    ));
    xml.push_str(&postal_address(who));
    if let Some(vat) = &who.vat_number {
        xml.push_str(&format!(
            "<cac:PartyTaxScheme><cbc:CompanyID>{}</cbc:CompanyID><cac:TaxScheme><cbc:ID>VAT</cbc:ID></cac:TaxScheme></cac:PartyTaxScheme>",
            esc(vat)
        ));
    }
    xml.push_str(&format!(
        "<cac:PartyLegalEntity><cbc:RegistrationName>{}</cbc:RegistrationName>",
        esc(&who.name)
    ));
    if let Some(number) = who
        .company_number
        .as_deref()
        .filter(|_| who.country == "BE")
    {
        xml.push_str(&format!(
            "<cbc:CompanyID schemeID=\"0208\">{}</cbc:CompanyID>",
            esc(number)
        ));
    }
    xml.push_str("</cac:PartyLegalEntity>");
    if let Some(email) = &who.email {
        xml.push_str(&format!(
            "<cac:Contact><cbc:ElectronicMail>{}</cbc:ElectronicMail></cac:Contact>",
            esc(email)
        ));
    }
    xml.push_str(&format!("</cac:Party></cac:{tag}>"));
    xml
}

fn tax_category(invoice: &Invoice, rate: Decimal, element: &str) -> String {
    let code = category(rate, invoice);
    let exemption = if code == "AE" {
        "<cbc:TaxExemptionReasonCode>VATEX-EU-AE</cbc:TaxExemptionReasonCode>"
    } else {
        ""
    };
    format!(
        "<cac:{element}><cbc:ID>{code}</cbc:ID><cbc:Percent>{}</cbc:Percent>{exemption}<cac:TaxScheme><cbc:ID>VAT</cbc:ID></cac:TaxScheme></cac:{element}>",
        percent(rate)
    )
}

fn tax_total(invoice: &Invoice, vat: &[VatSubtotal], currency: &str) -> String {
    let mut xml = format!(
        "<cac:TaxTotal><cbc:TaxAmount currencyID=\"{currency}\">{}</cbc:TaxAmount>",
        amount(invoice.tax_total)
    );
    for rate in vat {
        xml.push_str(&format!(
            "<cac:TaxSubtotal><cbc:TaxableAmount currencyID=\"{currency}\">{}</cbc:TaxableAmount><cbc:TaxAmount currencyID=\"{currency}\">{}</cbc:TaxAmount>{}</cac:TaxSubtotal>",
            amount(rate.taxable),
            amount(rate.tax),
            tax_category(invoice, rate.rate, "TaxCategory")
        ));
    }
    xml.push_str("</cac:TaxTotal>");
    xml
}

/// An invoice line; a negative line (a discount) has a negative quantity and
/// a positive price, as prices may not be negative (BR-27)
fn line(invoice: &Invoice, number: usize, line: &InvoiceLine, currency: &str) -> String {
    let negative = line.net < Decimal::ZERO;
    let quantity = if negative {
        -line.quantity.abs()
    } else {
        line.quantity
    };
    let sku = line
        .sku
        .as_deref()
        .map(|s| format!("<cac:SellersItemIdentification><cbc:ID>{}</cbc:ID></cac:SellersItemIdentification>", esc(s)))
        .unwrap_or_default();
    format!(
        "<cac:InvoiceLine><cbc:ID>{number}</cbc:ID><cbc:InvoicedQuantity unitCode=\"C62\">{quantity}</cbc:InvoicedQuantity>\
         <cbc:LineExtensionAmount currencyID=\"{currency}\">{}</cbc:LineExtensionAmount>\
         <cac:Item><cbc:Name>{}</cbc:Name>{sku}{}</cac:Item>\
         <cac:Price><cbc:PriceAmount currencyID=\"{currency}\">{}</cbc:PriceAmount></cac:Price></cac:InvoiceLine>",
        amount(line.net),
        esc(&line.description),
        tax_category(invoice, line.vat_rate, "ClassifiedTaxCategory"),
        amount(line.unit_price.abs())
    )
}

fn payment(invoice: &Invoice) -> String {
    let seller = &invoice.seller;
    let account = seller
        .iban
        .as_deref()
        .map(|iban| {
            let bic = seller
                .bic
                .as_deref()
                .map(|b| format!("<cac:FinancialInstitutionBranch><cbc:ID>{}</cbc:ID></cac:FinancialInstitutionBranch>", esc(b)))
                .unwrap_or_default();
            format!("<cac:PayeeFinancialAccount><cbc:ID>{}</cbc:ID>{bic}</cac:PayeeFinancialAccount>", esc(&iban.replace(' ', "")))
        })
        .unwrap_or_default();
    format!(
        "<cac:PaymentMeans><cbc:PaymentMeansCode>58</cbc:PaymentMeansCode><cbc:PaymentID>{}</cbc:PaymentID>{account}</cac:PaymentMeans>",
        esc(&invoice.payment_reference)
    )
}

/// The invoice as a Peppol BIS Billing 3.0 UBL document, sent by `sender` to
/// `recipient` (participant ids), with its PDF embedded when given
pub fn invoice_xml(invoice: &Invoice, sender: &str, recipient: &str, pdf: Option<&[u8]>) -> String {
    let currency = esc(&invoice.currency);
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <Invoice xmlns=\"urn:oasis:names:specification:ubl:schema:xsd:Invoice-2\" \
         xmlns:cac=\"urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2\" \
         xmlns:cbc=\"urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2\">",
    );
    xml.push_str(&format!(
        "<cbc:CustomizationID>{CUSTOMIZATION}</cbc:CustomizationID><cbc:ProfileID>{PROFILE}</cbc:ProfileID>\
         <cbc:ID>{}</cbc:ID><cbc:IssueDate>{}</cbc:IssueDate><cbc:DueDate>{}</cbc:DueDate>\
         <cbc:InvoiceTypeCode>380</cbc:InvoiceTypeCode><cbc:DocumentCurrencyCode>{currency}</cbc:DocumentCurrencyCode>\
         <cbc:BuyerReference>{}</cbc:BuyerReference><cac:OrderReference><cbc:ID>{}</cbc:ID></cac:OrderReference>",
        esc(&invoice.number),
        invoice.issue_date,
        invoice.due_date,
        esc(&invoice.order_number),
        esc(&invoice.order_number)
    ));
    if let Some(pdf) = pdf {
        xml.push_str(&format!(
            "<cac:AdditionalDocumentReference><cbc:ID>{0}</cbc:ID><cac:Attachment>\
             <cbc:EmbeddedDocumentBinaryObject mimeCode=\"application/pdf\" filename=\"{0}.pdf\">{1}</cbc:EmbeddedDocumentBinaryObject>\
             </cac:Attachment></cac:AdditionalDocumentReference>",
            esc(&invoice.number),
            base64::engine::general_purpose::STANDARD.encode(pdf)
        ));
    }
    xml.push_str(&party("AccountingSupplierParty", &invoice.seller, sender));
    xml.push_str(&party("AccountingCustomerParty", &invoice.buyer, recipient));
    xml.push_str(&payment(invoice));
    xml.push_str(&tax_total(invoice, &invoice.vat, &currency));
    let lines: Decimal = invoice.lines.iter().map(|l| l.net).sum();
    xml.push_str(&format!(
        "<cac:LegalMonetaryTotal><cbc:LineExtensionAmount currencyID=\"{currency}\">{0}</cbc:LineExtensionAmount>\
         <cbc:TaxExclusiveAmount currencyID=\"{currency}\">{0}</cbc:TaxExclusiveAmount>\
         <cbc:TaxInclusiveAmount currencyID=\"{currency}\">{1}</cbc:TaxInclusiveAmount>\
         <cbc:PrepaidAmount currencyID=\"{currency}\">{2}</cbc:PrepaidAmount>\
         <cbc:PayableAmount currencyID=\"{currency}\">{3}</cbc:PayableAmount></cac:LegalMonetaryTotal>",
        amount(lines),
        amount(lines + invoice.tax_total),
        amount(invoice.paid_amount),
        amount(lines + invoice.tax_total - invoice.paid_amount)
    ));
    for (i, l) in invoice.lines.iter().enumerate() {
        xml.push_str(&line(invoice, i + 1, l, &currency));
    }
    xml.push_str("</Invoice>");
    xml
}

/// What would make an access point refuse the invoice (empty: fine)
pub fn check(invoice: &Invoice) -> Vec<String> {
    let mut problems = Vec::new();
    let seller = &invoice.seller;
    let mut need = |ok: bool, what: &str| {
        if !ok {
            problems.push(what.to_string());
        }
    };
    need(!seller.name.is_empty(), "invoicing.seller.name is missing");
    need(
        seller.vat_number.is_some(),
        "invoicing.seller.vat_number is missing",
    );
    need(
        !seller.street.is_empty() && !seller.city.is_empty() && !seller.postcode.is_empty(),
        "the seller's address is incomplete",
    );
    need(
        seller.country.len() == 2,
        "invoicing.seller.country must be a two-letter code",
    );
    need(
        invoice.amount_due().is_zero() || seller.iban.is_some(),
        "invoicing.seller.iban is needed for an amount due",
    );
    need(!invoice.buyer.name.is_empty(), "the buyer has no name");
    need(
        invoice.buyer.country.len() == 2,
        "the buyer's country is missing",
    );
    need(!invoice.lines.is_empty(), "the invoice has no line");
    let lines: Decimal = invoice.lines.iter().map(|l| l.net).sum();
    need(
        lines.round_dp(2) == invoice.subtotal,
        "the lines do not add up to the subtotal",
    );
    problems
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{NaiveDate, Utc};
    use rust_decimal_macros::dec;
    use sqlx::types::Json;
    use uuid::Uuid;

    pub fn sample() -> Invoice {
        let seller = Party {
            name: "Store & Co".into(),
            vat_number: Some("BE0123456749".into()),
            company_number: Some("0123456749".into()),
            street: "Rue du Commerce 1".into(),
            city: "Namur".into(),
            postcode: "5000".into(),
            country: "BE".into(),
            iban: Some("BE68 5390 0754 7034".into()),
            ..Default::default()
        };
        let buyer = Party {
            name: "Builders SRL".into(),
            vat_number: Some("BE0999999920".into()),
            company_number: Some("0999999920".into()),
            street: "Rue Neuve 2".into(),
            city: "Liège".into(),
            postcode: "4000".into(),
            country: "BE".into(),
            ..Default::default()
        };
        let lines = vec![
            InvoiceLine {
                description: "Bricks <red>".into(),
                sku: Some("ST-1".into()),
                quantity: 10,
                unit_price: dec!(3.30),
                vat_rate: dec!(21),
                net: dec!(33.00),
            },
            InvoiceLine {
                description: "Discount".into(),
                sku: None,
                quantity: 1,
                unit_price: dec!(-3.00),
                vat_rate: dec!(21),
                net: dec!(-3.00),
            },
        ];
        let vat = crate::invoicing::vat_breakdown(&lines);
        Invoice {
            id: Uuid::nil(),
            number: "F2026-00001".into(),
            kind: "invoice".into(),
            order_id: Uuid::nil(),
            order_number: "ORD-1".into(),
            customer_id: None,
            company_id: None,
            issue_date: NaiveDate::from_ymd_opt(2026, 11, 10).unwrap(),
            due_date: NaiveDate::from_ymd_opt(2026, 12, 10).unwrap(),
            currency: "EUR".into(),
            seller: Json(seller),
            buyer: Json(buyer),
            subtotal: dec!(30.00),
            tax_total: vat[0].tax,
            total: dec!(30.00) + vat[0].tax,
            lines: Json(lines),
            vat: Json(vat),
            paid_amount: Decimal::ZERO,
            payment_reference: "+++202/6000/00198+++".into(),
            locale: "fr".into(),
            peppol_status: Some("pending".into()),
            peppol_message_id: None,
            peppol_recipient: None,
            peppol_error: None,
            peppol_updated_at: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn a_peppol_invoice_has_the_parts_access_points_require() {
        let invoice = sample();
        assert!(check(&invoice).is_empty(), "{:?}", check(&invoice));
        let xml = invoice_xml(
            &invoice,
            "0208:0123456749",
            "0208:0999999920",
            Some(b"%PDF-1.7"),
        );
        for part in [
            "<cbc:CustomizationID>urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0</cbc:CustomizationID>",
            "<cbc:ID>F2026-00001</cbc:ID>",
            "<cbc:EndpointID schemeID=\"0208\">0999999920</cbc:EndpointID>",
            "<cbc:Name>Store &amp; Co</cbc:Name>",
            "<cbc:Name>Bricks &lt;red&gt;</cbc:Name>",
            "<cbc:PaymentID>+++202/6000/00198+++</cbc:PaymentID>",
            "<cbc:ID>BE68539007547034</cbc:ID>",
            "<cbc:TaxableAmount currencyID=\"EUR\">30.00</cbc:TaxableAmount><cbc:TaxAmount currencyID=\"EUR\">6.30</cbc:TaxAmount>",
            "<cbc:InvoicedQuantity unitCode=\"C62\">-1</cbc:InvoicedQuantity><cbc:LineExtensionAmount currencyID=\"EUR\">-3.00</cbc:LineExtensionAmount>",
            "<cbc:PriceAmount currencyID=\"EUR\">3.00</cbc:PriceAmount>",
            "<cbc:PayableAmount currencyID=\"EUR\">36.30</cbc:PayableAmount>",
            "mimeCode=\"application/pdf\" filename=\"F2026-00001.pdf\">JVBERi0xLjc=<",
        ] {
            assert!(xml.contains(part), "missing {part}");
        }
        // Element order within the document follows the UBL schema
        let order = [
            "<cbc:DueDate>",
            "<cbc:InvoiceTypeCode>",
            "<cbc:BuyerReference>",
            "<cac:AdditionalDocumentReference>",
            "<cac:AccountingSupplierParty>",
            "<cac:AccountingCustomerParty>",
            "<cac:PaymentMeans>",
            "<cac:TaxTotal>",
            "<cac:LegalMonetaryTotal>",
            "<cac:InvoiceLine>",
        ];
        let positions: Vec<usize> = order.iter().map(|tag| xml.find(tag).unwrap()).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn missing_seller_details_are_reported() {
        let mut invoice = sample();
        invoice.seller.0.vat_number = None;
        invoice.seller.0.iban = None;
        let problems = check(&invoice);
        assert_eq!(problems.len(), 2, "{problems:?}");
        invoice.buyer.0.country = "FR".into();
        invoice.lines.0[0].vat_rate = Decimal::ZERO;
        assert_eq!(category(Decimal::ZERO, &invoice), "AE");
    }
}
