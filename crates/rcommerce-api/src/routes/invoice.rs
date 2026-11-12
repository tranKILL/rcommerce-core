//! Invoice HTML generator for rCommerce orders.
//!
//! Generates a beautiful, print-friendly HTML invoice that can be
//! saved as PDF from the browser.

pub use rcommerce_core::invoice::{format_address, InvoiceAddress, InvoiceData, InvoiceItem};
use rcommerce_core::models::{Order, PaymentStatus};
use rcommerce_core::notification::i18n;

/// How the order is paid, in `locale`: the method when known, else where the
/// payment stands
fn payment_text(order: &Order, locale: &str) -> String {
    if let Some(method) = order.payment_method.as_deref().filter(|m| !m.is_empty()) {
        return rcommerce_core::invoice::display_shipping_method(method);
    }
    let key = match order.payment_status {
        PaymentStatus::Paid => "paid",
        PaymentStatus::Pending => "awaiting_payment",
        PaymentStatus::Authorized => "payment_authorized",
        PaymentStatus::Failed => "payment_failed",
        PaymentStatus::Cancelled => "payment_cancelled",
        PaymentStatus::Refunded => "refunded",
    };
    i18n::text(locale, "order_document", key)
}

/// How the order is handed over, in `locale`: collected at a store location
/// (with the day it is ready) or delivered
fn fulfilment_text(order: &Order, locale: &str) -> Option<String> {
    let t = |key: &str| i18n::text(locale, "order_document", key);
    let metadata = order.metadata.as_ref()?;
    if let Some(pickup) = metadata.get("pickup") {
        let place = pickup.get("location_name")?.as_str()?;
        let ready = pickup
            .get("ready_at")
            .and_then(|v| v.as_str())
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok());
        return Some(match ready {
            Some(at) => t("pickup_ready")
                .replace("{place}", place)
                .replace("{date}", &i18n::date(locale, at.date_naive())),
            None => t("pickup").replace("{place}", place),
        });
    }
    metadata.get("delivery").map(|_| t("delivery"))
}

/// Build invoice data from an order row and its items, in `locale`.
pub fn invoice_data_from_order(
    order: &rcommerce_core::models::Order,
    items: &[rcommerce_core::order::OrderItem],
    branding: &rcommerce_core::BrandingConfig,
    locale: &str,
) -> InvoiceData {
    let billing = order.billing_address.as_ref().map(|v| InvoiceAddress {
        first_name: v
            .get("first_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        last_name: v
            .get("last_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        address_1: v
            .get("address1")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("address_1").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string(),
        address_2: v
            .get("address2")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("address_2").and_then(|v| v.as_str()))
            .map(|s| s.to_string()),
        city: v
            .get("city")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        state: v
            .get("state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        postcode: v
            .get("zip")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("postcode").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string(),
        country: v
            .get("country")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        phone: v
            .get("phone")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    });

    let shipping = order.shipping_address.as_ref().map(|v| InvoiceAddress {
        first_name: v
            .get("first_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        last_name: v
            .get("last_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        address_1: v
            .get("address1")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("address_1").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string(),
        address_2: v
            .get("address2")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("address_2").and_then(|v| v.as_str()))
            .map(|s| s.to_string()),
        city: v
            .get("city")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        state: v
            .get("state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        postcode: v
            .get("zip")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("postcode").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string(),
        country: v
            .get("country")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        phone: v
            .get("phone")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    });

    // Guest checkouts often have no billing address snapshot; fall back to
    // the shipping address so Bill To never renders "No address provided".
    let billing = billing.or_else(|| shipping.clone());

    // Bundle component lines ($0 stock-reservation rows) are internal only;
    // the parent bundle line carries the price shown on the invoice.
    let invoice_items: Vec<_> = items
        .iter()
        .filter(|i| i.is_bundle_component != Some(true))
        .map(|i| InvoiceItem {
            name: i.name.clone(),
            variant_title: i.variant_name.clone(),
            sku: i.sku.clone(),
            quantity: i.quantity,
            unit_price: format!("{:.2}", i.price),
            // Excluding VAT, like the subtotal they add up to
            total: format!("{:.2}", i.subtotal),
            weight: i.weight.map(|w| {
                let unit = i
                    .weight_unit
                    .map(|u| format!("{:?}", u).to_lowercase())
                    .unwrap_or_else(|| "kg".to_string());
                format!("{}{}", w, unit)
            }),
        })
        .collect();

    InvoiceData {
        logo: branding.pdf_logo.clone(),
        company_lines: branding.address_lines.clone(),
        order_number: order.order_number.clone(),
        order_date: i18n::date(locale, order.created_at.date_naive()),
        status: format!("{:?}", order.status).to_lowercase(),
        currency: order.currency.to_string(),
        customer_email: order.customer_email.clone(),
        billing,
        shipping,
        items: invoice_items,
        subtotal: format!("{:.2}", order.subtotal),
        discount_total: (order.discount_total > rust_decimal::Decimal::ZERO)
            .then(|| format!("{:.2}", order.discount_total)),
        coupon_code: order.coupon_code.clone(),
        shipping_total: format!("{:.2}", order.shipping_total),
        tax_total: format!("{:.2}", order.tax_total),
        total: format!("{:.2}", order.total),
        payment_method: payment_text(order, locale),
        shipping_method: order.shipping_method.clone(),
        locale: locale.to_string(),
        summary_only: branding.issues_invoices,
        fulfilment: fulfilment_text(order, locale),
        brand_name: branding.name.clone(),
        brand_email: branding.support_email.clone(),
    }
}

/// Generate an HTML invoice.
pub fn generate_invoice_html(data: &InvoiceData) -> Result<String, String> {
    let brand = if data.brand_name.is_empty() {
        "rCommerce"
    } else {
        data.brand_name.as_str()
    };
    let contact = if data.brand_email.is_empty() {
        "support@example.com"
    } else {
        data.brand_email.as_str()
    };
    let billing_lines = data
        .billing
        .as_ref()
        .map(|a| format_address(a))
        .unwrap_or_default();
    let shipping_lines = data
        .shipping
        .as_ref()
        .map(|a| format_address(a))
        .unwrap_or_default();

    let items_html: String = data
        .items
        .iter()
        .map(|item| {
            let sku_line = item.sku.as_deref().unwrap_or("");
            let variant_line = item
                .variant_title
                .as_deref()
                .filter(|v| !v.is_empty())
                .map(|v| {
                    format!(
                        r#"<div class="item-variant">{variant}</div>"#,
                        variant = escape_html(v)
                    )
                })
                .unwrap_or_default();
            format!(
                r#"<tr>
                <td class="item-name">{name}{variant}</td>
                <td class="item-sku">{sku}</td>
                <td class="item-qty">{qty}</td>
                <td class="item-price">{price}</td>
                <td class="item-total">{total}</td>
            </tr>"#,
                name = escape_html(&item.name),
                variant = variant_line,
                sku = escape_html(sku_line),
                qty = item.quantity,
                price = data.money(&item.unit_price),
                total = data.money(&item.total),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let t = |key: &str| escape_html(&data.text(key));
    let status_key = match data.status.as_str() {
        "onhold" => "status_on_hold".to_string(),
        other => format!("status_{other}"),
    };
    let payment_row = if data.payment_method.is_empty() {
        String::new()
    } else {
        format!(
            "<div><strong>{}:</strong> {}</div>",
            t("payment"),
            escape_html(&data.payment_method)
        )
    };
    let fulfilment_row = data
        .fulfilment
        .as_deref()
        .map(|line| format!("<div>{}</div>", escape_html(line)))
        .unwrap_or_default();
    let summary_note = if data.summary_only {
        format!(r#"<p style="margin-top: 4px;">{}</p>"#, t("not_an_invoice"))
    } else {
        String::new()
    };

    // Discount row: only when a discount was applied, shown as a negative
    // amount in green, labelled with the coupon code when known.
    let discount_row = data
        .discount_total
        .as_deref()
        .and_then(|d| d.parse::<rust_decimal::Decimal>().ok())
        .filter(|d| *d > rust_decimal::Decimal::ZERO)
        .map(|d| {
            let label = match data.coupon_code.as_deref() {
                Some(code) if !code.is_empty() => data.text("discount_code").replace("{code}", code),
                _ => data.text("discount"),
            };
            format!(
                r#"<div class="total-row discount"><span>{label}</span><span style="color:#059669;">{amount}</span></div>"#,
                label = escape_html(&label),
                amount = data.money(&(-d).to_string()),
            )
        })
        .unwrap_or_default();

    Ok(format!(
        r#"<!DOCTYPE html>
<html lang="{lang}">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{title} {order_number}</title>
<style>
  @import url('https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600;700&display=swap');
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ font-family: 'Inter', -apple-system, sans-serif; color: #1a1a1a; background: #f5f5f5; padding: 40px; }}
  .invoice {{ max-width: 800px; margin: 0 auto; background: white; padding: 48px; box-shadow: 0 2px 20px rgba(0,0,0,0.08); }}
  .header {{ display: flex; justify-content: space-between; align-items: flex-start; margin-bottom: 32px; padding-bottom: 20px; border-bottom: 2px solid #1a1a1a; }}
  .brand {{ font-size: 28px; font-weight: 700; letter-spacing: -0.5px; }}
  .brand span {{ color: #ff4d00; }}
  .invoice-label {{ font-size: 14px; font-weight: 600; text-transform: uppercase; letter-spacing: 2px; color: #666; }}
  .meta {{ display: grid; grid-template-columns: 1fr 1fr; gap: 32px; margin-bottom: 28px; }}
  .meta-label {{ font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 1.5px; color: #999; margin-bottom: 10px; }}
  .meta-value {{ font-size: 14px; line-height: 1.7; }}
  .status-badge {{ display: inline-block; padding: 4px 12px; border-radius: 4px; font-size: 12px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; }}
  .status-completed {{ background: #ecfdf5; color: #059669; }}
  .status-processing {{ background: #eff6ff; color: #2563eb; }}
  .status-confirmed {{ background: #eff6ff; color: #2563eb; }}
  .status-pending {{ background: #fffbeb; color: #d97706; }}
  .status-cancelled {{ background: #fef2f2; color: #dc2626; }}
  .items-table {{ width: 100%; border-collapse: collapse; margin-bottom: 28px; }}
  .items-table th {{ text-align: left; font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 1px; color: #999; padding: 10px 12px 10px 0; border-bottom: 2px solid #e5e5e5; }}
  .items-table td {{ padding: 14px 12px 14px 0; border-bottom: 1px solid #f0f0f0; font-size: 14px; vertical-align: top; }}
  .item-name {{ font-weight: 500; line-height: 1.4; }}
  .item-variant {{ font-weight: 400; font-size: 12px; color: #666; margin-top: 2px; }}
  .item-sku {{ color: #888; font-size: 12px; white-space: nowrap; }}
  .item-qty {{ text-align: center; white-space: nowrap; }}
  .item-price {{ text-align: right; white-space: nowrap; }}
  .item-total {{ text-align: right; white-space: nowrap; font-weight: 500; }}
  .totals {{ display: flex; justify-content: flex-end; margin-bottom: 40px; }}
  .totals-box {{ width: 300px; }}
  .total-row {{ display: flex; justify-content: space-between; padding: 10px 0; font-size: 14px; }}
  .total-row.discount {{ color: #059669; }}
  .total-row.grand {{ font-size: 18px; font-weight: 700; border-top: 2px solid #1a1a1a; margin-top: 8px; padding-top: 14px; }}
  .footer {{ margin-top: 48px; padding-top: 20px; border-top: 1px solid #e5e5e5; text-align: center; font-size: 12px; color: #999; }}
  @media print {{
    body {{ background: white; padding: 0; }}
    .invoice {{ box-shadow: none; padding: 24px; max-width: none; }}
  }}
</style>
</head>
<body>
<div class="invoice">
  <div class="header">
    <div>
      <div class="brand">{brand}<span>.</span></div>
    </div>
    <div style="text-align: right;">
      <div class="invoice-label">{title}</div>
      <div style="font-size: 13px; color: #666; margin-top: 4px;">{order_number}</div>
    </div>
  </div>

  <div class="meta">
    <div class="meta-block">
      <div class="meta-label">{l_details}</div>
      <div class="meta-value">
        <div><strong>{l_order}:</strong> {order_number}</div>
        <div><strong>{l_date}:</strong> {order_date}</div>
        <div><strong>{l_status}:</strong> <span class="status-badge status-{status_class}">{status}</span></div>
        {payment_row}
        {fulfilment_row}
      </div>
    </div>
    <div class="meta-block">
      <div class="meta-label">{l_contact}</div>
      <div class="meta-value">
        <div>{customer_email}</div>
      </div>
    </div>
  </div>

  <div class="meta">
    <div class="meta-block">
      <div class="meta-label">{l_billing}</div>
      <div class="meta-value">{billing_html}</div>
    </div>
    <div class="meta-block">
      <div class="meta-label">{l_shipping_address}</div>
      <div class="meta-value">{shipping_html}</div>
    </div>
  </div>

  <table class="items-table">
    <thead>
      <tr>
        <th>{l_product}</th>
        <th>{l_sku}</th>
        <th style="text-align:center">{l_qty}</th>
        <th style="text-align:right">{l_price}</th>
        <th style="text-align:right">{l_total}</th>
      </tr>
    </thead>
    <tbody>
      {items_html}
    </tbody>
  </table>

  <div class="totals">
    <div class="totals-box">
      <div class="total-row">
        <span>{l_subtotal}</span><span>{subtotal}</span>
      </div>
      {discount_row}
      <div class="total-row">
        <span>{l_shipping}</span><span>{shipping_total}</span>
      </div>
      <div class="total-row">
        <span>{l_tax}</span><span>{tax_total}</span>
      </div>
      <div class="total-row grand">
        <span>{l_total}</span><span>{total}</span>
      </div>
    </div>
  </div>

  <div class="footer">
    <p>{thanks}</p>
    {summary_note}
    <p style="margin-top: 4px;">{brand}</p>
  </div>
</div>
</body>
</html>"#,
        order_number = data.order_number,
        order_date = data.order_date,
        status = t(&status_key),
        status_class = data.status.to_lowercase(),
        payment_row = payment_row,
        fulfilment_row = fulfilment_row,
        summary_note = summary_note,
        lang = escape_html(&data.locale),
        title = t(if data.summary_only {
            "title_summary"
        } else {
            "title_invoice"
        }),
        l_details = t("order_details"),
        l_order = t("order_number"),
        l_date = t("order_date"),
        l_status = t("status"),
        l_contact = t("contact"),
        l_billing = t("billing_address"),
        l_shipping_address = t("shipping_address"),
        l_product = t("product"),
        l_sku = t("sku"),
        l_qty = t("qty"),
        l_price = t("price"),
        l_total = t("total"),
        l_subtotal = t("subtotal"),
        l_shipping = t("shipping"),
        l_tax = t("tax"),
        thanks = escape_html(&data.text("thanks").replace("{email}", contact)),
        customer_email = data.customer_email,
        billing_html = format_address_html(&billing_lines, &t("no_address")),
        shipping_html = format_address_html(&shipping_lines, &t("no_address")),
        items_html = items_html,
        discount_row = discount_row,
        subtotal = data.money(&data.subtotal),
        shipping_total = data.money(&data.shipping_total),
        tax_total = data.money(&data.tax_total),
        total = data.money(&data.total),
    ))
}

fn format_address_html(lines: &[String], none: &str) -> String {
    if lines.is_empty() {
        return format!("<em style='color:#999'>{none}</em>");
    }
    lines
        .iter()
        .map(|l| format!("<div>{}</div>", escape_html(l)))
        .collect::<Vec<_>>()
        .join("")
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invoice_items_exclude_bundle_components() {
        use chrono::Utc;
        use rust_decimal::Decimal;

        let order = rcommerce_core::models::Order {
            id: uuid::Uuid::new_v4(),
            order_number: "ORD-BUNDLE-1".to_string(),
            customer_id: None,
            customer_email: "customer@example.com".to_string(),
            currency: "USD".to_string(),
            order_type: rcommerce_core::models::OrderType::OneTime,
            subtotal: Decimal::new(12000, 2),
            tax_total: Decimal::ZERO,
            shipping_total: Decimal::new(1000, 2),
            discount_total: Decimal::new(1275, 2),
            total: Decimal::new(11725, 2),
            status: rcommerce_core::models::OrderStatus::Confirmed,
            fulfillment_status: rcommerce_core::models::FulfillmentStatus::Pending,
            payment_status: rcommerce_core::models::PaymentStatus::Paid,
            invoice_number: None,
            test: false,
            shipping_address_id: None,
            billing_address_id: None,
            payment_method: None,
            shipping_method: None,
            notes: None,
            tags: Vec::new(),
            draft: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            subscription_id: None,
            billing_cycle: None,
            shipping_address: None,
            billing_address: None,
            metadata: None,
            coupon_code: Some("1V97NIBP40".to_string()),
            coupon_id: None,
            ip_address: None,
            user_agent: None,
            cancelled_at: None,
        };

        let make_item =
            |name: &str, sku: &str, total: Decimal, component: bool, variant: Option<&str>| {
                rcommerce_core::order::OrderItem {
                    id: uuid::Uuid::new_v4(),
                    order_id: order.id,
                    product_id: uuid::Uuid::new_v4(),
                    variant_id: None,
                    quantity: 1,
                    price: total,
                    subtotal: total,
                    tax_amount: Decimal::ZERO,
                    total,
                    sku: Some(sku.to_string()),
                    name: name.to_string(),
                    variant_name: variant.map(|v| v.to_string()),
                    weight: None,
                    weight_unit: None,
                    is_bundle_component: Some(component),
                    metadata: serde_json::json!({}),
                    created_at: Utc::now(),
                }
            };
        let items = vec![
            make_item(
                "Race Pack",
                "RACE-PACK",
                Decimal::new(12000, 2),
                false,
                None,
            ),
            // $0 stock-reservation component line: must not appear on the invoice
            make_item("80mm Maverick Wheels", "MAV-80", Decimal::ZERO, true, None),
            // Variant line: must map order_items.variant_title through
            make_item(
                "Caliber 3 Precision Baseplate",
                "CALIBER3-50",
                Decimal::new(7500, 2),
                false,
                Some("Degree: 50"),
            ),
        ];

        let data = invoice_data_from_order(
            &order,
            &items,
            &rcommerce_core::BrandingConfig::default(),
            "en",
        );

        assert_eq!(data.items.len(), 2, "component line must be excluded");
        assert_eq!(data.items[0].name, "Race Pack");
        assert_eq!(data.items[0].total, "120.00");
        assert_eq!(data.items[1].name, "Caliber 3 Precision Baseplate");
        assert_eq!(
            data.items[1].variant_title.as_deref(),
            Some("Degree: 50"),
            "variant_title must carry the order item's variant"
        );

        // The rendered HTML invoice shows the variant under the item name.
        let html = generate_invoice_html(&data).expect("invoice html generation failed");
        assert!(html.contains(r#"<div class="item-variant">Degree: 50</div>"#));
        // And items without a variant render no variant line at all.
        let race_row = html
            .split("Race Pack")
            .nth(1)
            .expect("Race Pack row missing")
            .split("</tr>")
            .next()
            .expect("Race Pack row must close");
        assert!(
            !race_row.contains("item-variant"),
            "simple product must not render a variant line"
        );

        // Discount mapping + rendered row (negative, green, coupon labelled).
        assert_eq!(data.discount_total.as_deref(), Some("12.75"));
        assert_eq!(data.coupon_code.as_deref(), Some("1V97NIBP40"));
        assert!(
            html.contains("Discount (1V97NIBP40)"),
            "discount row must be labelled with the coupon code"
        );
        assert!(
            html.contains(r#"<span style="color:#059669;">-$12.75</span>"#),
            "discount amount must render as a negative green amount"
        );
        // Discount row sits between Subtotal and Shipping in the totals block.
        let sub_pos = html.find("Subtotal").expect("subtotal missing");
        let disc_pos = html
            .find("Discount (1V97NIBP40)")
            .expect("discount missing");
        let after_sub = &html[sub_pos..];
        let ship_pos = after_sub
            .find("Shipping")
            .expect("shipping missing in totals block");
        let disc_rel = disc_pos - sub_pos;
        assert!(
            disc_rel < ship_pos,
            "discount row must sit between Subtotal and Shipping"
        );
    }

    #[test]
    fn test_invoice_html_no_discount_no_row() {
        use chrono::Utc;
        use rust_decimal::Decimal;

        let order = rcommerce_core::models::Order {
            id: uuid::Uuid::new_v4(),
            order_number: "ORD-NODISC-1".to_string(),
            customer_id: None,
            customer_email: "customer@example.com".to_string(),
            currency: "USD".to_string(),
            order_type: rcommerce_core::models::OrderType::OneTime,
            subtotal: Decimal::new(5000, 2),
            tax_total: Decimal::ZERO,
            shipping_total: Decimal::new(1000, 2),
            discount_total: Decimal::ZERO,
            total: Decimal::new(6000, 2),
            status: rcommerce_core::models::OrderStatus::Confirmed,
            fulfillment_status: rcommerce_core::models::FulfillmentStatus::Pending,
            payment_status: rcommerce_core::models::PaymentStatus::Paid,
            invoice_number: None,
            test: false,
            shipping_address_id: None,
            billing_address_id: None,
            payment_method: None,
            shipping_method: None,
            notes: None,
            tags: Vec::new(),
            draft: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            subscription_id: None,
            billing_cycle: None,
            shipping_address: None,
            billing_address: None,
            metadata: None,
            coupon_code: None,
            coupon_id: None,
            ip_address: None,
            user_agent: None,
            cancelled_at: None,
        };

        let data = invoice_data_from_order(
            &order,
            &[],
            &rcommerce_core::BrandingConfig::default(),
            "en",
        );
        assert_eq!(data.discount_total, None);
        let html = generate_invoice_html(&data).expect("invoice html generation failed");
        assert!(!html.contains("Discount"), "no discount row must render");

        let branding = rcommerce_core::BrandingConfig {
            issues_invoices: true,
            ..Default::default()
        };
        let french = generate_invoice_html(&invoice_data_from_order(&order, &[], &branding, "fr"))
            .expect("french html");
        assert!(french.contains(r#"<html lang="fr">"#));
        assert!(french.contains("Bon de commande") && french.contains("n'est pas une facture"));
        assert!(french.contains("Payé") && !french.contains("Invoice"));
    }
}
