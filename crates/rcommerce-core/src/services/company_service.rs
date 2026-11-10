//! Business customers: a company with a checked VAT number, its members, and
//! the customer group its prices come from.
//!
//! A signed-in customer opens a company account by giving its VAT number. The
//! number is checked (format and check digits, then the EU VIES register unless
//! the store checks the format only) and the account is usable at once: the
//! company joins the `trade` customer group, which price lists can target. The
//! customer becomes its owner and can add other customers as buyers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::list_sort::{order_by, SortColumns};
use crate::tax::{VatId, ViesValidator};
use crate::{Error, Result};

/// The customer group every new company joins
pub const TRADE_GROUP: &str = "trade";

/// How a VAT number is checked when a company signs up
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VatCheckMode {
    /// Format, check digits, then the EU VIES register (default)
    #[default]
    Vies,
    /// Format and check digits only (development, tests, stores outside the EU)
    Format,
}

/// A company account
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Company {
    pub id: Uuid,
    pub name: String,
    /// Country prefix and number, e.g. `BE0123456749`
    pub vat_number: String,
    pub country: String,
    /// National company number (in Belgium the VAT number's digits)
    pub company_number: Option<String>,
    /// Registered address as the VAT register gives it
    pub address: Option<String>,
    pub customer_group_id: Option<Uuid>,
    /// `active` or `suspended` (a suspended company gets public prices)
    pub status: String,
    /// When the VAT register confirmed the number (`None` = format check only)
    pub vat_checked_at: Option<DateTime<Utc>>,
    /// Peppol participant id, when the company receives e-invoices
    pub peppol_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A customer belonging to a company
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CompanyMember {
    pub customer_id: Uuid,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    /// `owner` (manages members) or `buyer`
    pub role: String,
    pub created_at: DateTime<Utc>,
}

/// A group of customers that price lists can target
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CustomerGroup {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    /// Members see prices excluding VAT first
    pub prices_exclude_tax: bool,
    pub created_at: DateTime<Utc>,
}

/// What a customer's prices depend on
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PricingContext {
    /// Their active company, if any
    pub company_id: Option<Uuid>,
    /// Their company's group; their own group when not in a company
    pub customer_group_id: Option<Uuid>,
}

/// A VAT number that passed the check
#[derive(Debug, Clone)]
pub struct CheckedVat {
    pub vat: VatId,
    /// Name and address from the VAT register, when it gives them
    pub name: Option<String>,
    pub address: Option<String>,
    pub registered: bool,
}

/// Fields staff may change on a company; `None` leaves a field as it is
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompanyUpdate {
    pub name: Option<String>,
    pub status: Option<String>,
    /// Moves the company to another customer group
    pub customer_group_id: Option<Uuid>,
}

/// Columns the company list can be sorted on
const COMPANY_SORTS: SortColumns = &[
    ("since", "created_at"),
    ("name", "lower(name)"),
    ("vat_number", "vat_number"),
    ("status", "status"),
];

/// Search of the staff company list
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompanyQuery {
    /// Part of the name or VAT number
    pub q: Option<String>,
    pub status: Option<String>,
    /// `since` (default `-since`), `name`, `vat_number` or `status`; `-` in
    /// front for descending order
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

/// Company accounts and customer groups
#[derive(Clone)]
pub struct CompanyService {
    pool: PgPool,
    mode: VatCheckMode,
    vies: std::sync::Arc<ViesValidator>,
}

/// Whether the Belgian check digits of a 10-digit number are right:
/// the last two digits are 97 minus the first eight modulo 97
pub fn belgian_check_digits_ok(number: &str) -> bool {
    if number.len() != 10 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let (body, check) = number.split_at(8);
    match (body.parse::<u64>(), check.parse::<u64>()) {
        (Ok(body), Ok(check)) => 97 - body % 97 == check,
        _ => false,
    }
}

/// Parse a VAT number as a client typed it (`be 0123.456.749`), padding old
/// 9-digit Belgian numbers and checking Belgian check digits
pub fn parse_vat_number(raw: &str) -> Result<VatId> {
    let invalid = || Error::validation("vat_number is not a valid VAT number");
    let mut vat = VatId::parse(raw).map_err(|_| invalid())?;
    if vat.country_code == "BE" {
        if vat.number.len() == 9 {
            vat.number.insert(0, '0');
        }
        if !belgian_check_digits_ok(&vat.number) {
            return Err(invalid());
        }
    }
    Ok(vat)
}

/// The name given, else the VAT register's; required, at most 255 characters
fn company_name(given: Option<&str>, checked: &CheckedVat) -> Result<String> {
    let name = given
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or(checked.name.clone())
        .ok_or_else(|| Error::validation("name is required"))?;
    if name.chars().count() > 255 {
        return Err(Error::validation("name must be at most 255 characters"));
    }
    Ok(name)
}

const VAT_TAKEN: &str = "This VAT number already has a company account: ask its owner to add you";

fn conflict(message: &str) -> Error {
    Error::HttpError(http::StatusCode::CONFLICT, message.to_string())
}

impl CompanyService {
    /// `vies_url` replaces the EU VIES endpoint (a test double)
    pub fn new(pool: PgPool, mode: VatCheckMode, vies_url: Option<String>) -> Self {
        let vies = match vies_url {
            Some(url) => ViesValidator::new().with_base_url(url),
            None => ViesValidator::new(),
        };
        Self {
            pool,
            mode,
            vies: std::sync::Arc::new(vies),
        }
    }

    /// Check a VAT number: format and check digits, then (in `vies` mode) the
    /// EU register. An unregistered number is a validation error; a register
    /// that does not answer is a network error (503), to retry later.
    pub async fn check_vat(&self, raw: &str) -> Result<CheckedVat> {
        let vat = parse_vat_number(raw)?;
        if self.mode == VatCheckMode::Format {
            return Ok(CheckedVat {
                vat,
                name: None,
                address: None,
                registered: false,
            });
        }
        let result = self.vies.validate(&vat).await.map_err(|e| match e {
            Error::Validation(m) => Error::Validation(m),
            other => Error::Network(format!("VAT register unavailable: {other}")),
        })?;
        if !result.is_valid {
            return Err(Error::validation(
                "vat_number is not registered for VAT (EU VIES)",
            ));
        }
        Ok(CheckedVat {
            vat,
            name: result.business_name,
            address: result.business_address,
            registered: true,
        })
    }

    /// Open a company account for `owner`: check the VAT number, then create
    /// the company in the trade group with `owner` as its owner. `name`
    /// defaults to the name in the VAT register.
    pub async fn sign_up(
        &self,
        owner: Uuid,
        vat_number: &str,
        name: Option<&str>,
    ) -> Result<Company> {
        if self.company_of(owner).await?.is_some() {
            return Err(conflict("You already belong to a company account"));
        }
        let checked = self.check_vat(vat_number).await?;
        self.refuse_taken(&checked.vat).await?;
        let name = company_name(name, &checked)?;
        let mut tx = self.pool.begin().await?;
        let company = sqlx::query_as::<_, Company>(
            "INSERT INTO companies (name, vat_number, country, company_number, address, \
             customer_group_id, vat_checked_at) \
             VALUES ($1, $2, $3, $4, $5, (SELECT id FROM customer_groups WHERE code = $6), \
             CASE WHEN $7 THEN NOW() END) \
             ON CONFLICT (vat_number) DO NOTHING RETURNING *",
        )
        .bind(&name)
        .bind(checked.vat.full_id())
        .bind(&checked.vat.country_code)
        .bind((checked.vat.country_code == "BE").then(|| checked.vat.number.clone()))
        .bind(&checked.address)
        .bind(TRADE_GROUP)
        .bind(checked.registered)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| conflict(VAT_TAKEN))?;
        Self::insert_member(&mut tx, company.id, owner, "owner").await?;
        tx.commit().await?;
        Ok(company)
    }

    /// A VAT number that already has an account is refused before anything
    /// else is asked (the insert still guards against a race)
    async fn refuse_taken(&self, vat: &VatId) -> Result<()> {
        let taken: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM companies WHERE vat_number = $1)")
                .bind(vat.full_id())
                .fetch_one(&self.pool)
                .await?;
        if taken {
            return Err(conflict(VAT_TAKEN));
        }
        Ok(())
    }

    async fn insert_member(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        company: Uuid,
        customer: Uuid,
        role: &str,
    ) -> Result<()> {
        let added = sqlx::query(
            "INSERT INTO company_members (company_id, customer_id, role) VALUES ($1, $2, $3) \
             ON CONFLICT (customer_id) DO NOTHING",
        )
        .bind(company)
        .bind(customer)
        .bind(role)
        .execute(&mut **tx)
        .await?;
        if added.rows_affected() == 0 {
            return Err(conflict(
                "This customer already belongs to a company account",
            ));
        }
        Ok(())
    }

    /// The company a customer belongs to, with their role in it
    pub async fn company_of(&self, customer: Uuid) -> Result<Option<(Company, String)>> {
        let row: Option<(Uuid, String)> =
            sqlx::query_as("SELECT company_id, role FROM company_members WHERE customer_id = $1")
                .bind(customer)
                .fetch_optional(&self.pool)
                .await?;
        let Some((company_id, role)) = row else {
            return Ok(None);
        };
        Ok(self.get(company_id).await?.map(|company| (company, role)))
    }

    /// One company by id
    pub async fn get(&self, id: Uuid) -> Result<Option<Company>> {
        Ok(
            sqlx::query_as::<_, Company>("SELECT * FROM companies WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// The members of a company, owners first
    pub async fn members(&self, company: Uuid) -> Result<Vec<CompanyMember>> {
        Ok(sqlx::query_as::<_, CompanyMember>(
            "SELECT m.customer_id, c.email, c.first_name, c.last_name, m.role, m.created_at \
             FROM company_members m JOIN customers c ON c.id = m.customer_id \
             WHERE m.company_id = $1 ORDER BY m.role = 'owner' DESC, m.created_at",
        )
        .bind(company)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The company `owner` owns; refused when they are not an owner
    async fn owned_company(&self, owner: Uuid) -> Result<Company> {
        match self.company_of(owner).await? {
            Some((company, role)) if role == "owner" => Ok(company),
            Some(_) => Err(Error::forbidden(
                "Only the company's owner manages its members",
            )),
            None => Err(Error::not_found("You do not belong to a company account")),
        }
    }

    /// Add an existing customer (by e-mail) to the owner's company as a buyer
    pub async fn add_member(&self, owner: Uuid, email: &str) -> Result<CompanyMember> {
        let company = self.owned_company(owner).await?;
        let customer: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM customers WHERE LOWER(email) = LOWER($1)")
                .bind(email.trim())
                .fetch_optional(&self.pool)
                .await?;
        let customer = customer.ok_or_else(|| {
            Error::not_found("No customer account uses this e-mail: they must sign up first")
        })?;
        let mut tx = self.pool.begin().await?;
        Self::insert_member(&mut tx, company.id, customer, "buyer").await?;
        tx.commit().await?;
        self.members(company.id)
            .await?
            .into_iter()
            .find(|m| m.customer_id == customer)
            .ok_or_else(|| Error::internal("member just added is missing"))
    }

    /// Remove a buyer from the owner's company (an owner cannot remove themself)
    pub async fn remove_member(&self, owner: Uuid, customer: Uuid) -> Result<()> {
        let company = self.owned_company(owner).await?;
        if customer == owner {
            return Err(Error::validation(
                "The owner cannot leave their own company",
            ));
        }
        let removed =
            sqlx::query("DELETE FROM company_members WHERE company_id = $1 AND customer_id = $2")
                .bind(company.id)
                .bind(customer)
                .execute(&self.pool)
                .await?;
        if removed.rows_affected() == 0 {
            return Err(Error::not_found(
                "This customer is not a member of your company",
            ));
        }
        Ok(())
    }

    /// Companies for staff: newest first, filtered by name/VAT part and status;
    /// returns the page and the total count
    pub async fn list(&self, query: &CompanyQuery) -> Result<(Vec<Company>, i64)> {
        let per_page = query.per_page.unwrap_or(50).clamp(1, 100);
        let page = query.page.unwrap_or(1).max(1);
        let q = query
            .q
            .as_deref()
            .map(|q| format!("%{}%", q.trim().replace(['%', '_'], "")));
        let filter = "WHERE ($1::text IS NULL OR name ILIKE $1 OR vat_number ILIKE $1) \
                      AND ($2::text IS NULL OR status = $2)";
        let order = order_by(query.sort.as_deref(), COMPANY_SORTS, "-since", "id")?;
        let companies = sqlx::query_as::<_, Company>(&format!(
            "SELECT * FROM companies {filter} ORDER BY {order} LIMIT $3 OFFSET $4"
        ))
        .bind(&q)
        .bind(&query.status)
        .bind(per_page)
        .bind((page - 1) * per_page)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM companies {filter}"))
            .bind(&q)
            .bind(&query.status)
            .fetch_one(&self.pool)
            .await?;
        Ok((companies, total))
    }

    /// Change a company's name, status or customer group
    pub async fn update(&self, id: Uuid, update: CompanyUpdate) -> Result<Company> {
        if let Some(status) = update.status.as_deref() {
            if !matches!(status, "active" | "suspended") {
                return Err(Error::validation("status must be active or suspended"));
            }
        }
        let name = update.name.as_deref().map(str::trim);
        if name.is_some_and(|n| n.is_empty() || n.chars().count() > 255) {
            return Err(Error::validation("name must be 1 to 255 characters"));
        }
        sqlx::query_as::<_, Company>(
            "UPDATE companies SET name = COALESCE($2, name), status = COALESCE($3, status), \
             customer_group_id = COALESCE($4, customer_group_id), updated_at = NOW() \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(name)
        .bind(update.status)
        .bind(update.customer_group_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
                Error::validation("customer_group_id does not exist")
            }
            other => other.into(),
        })?
        .ok_or_else(|| Error::not_found("Company not found"))
    }

    /// Every customer group, by code
    pub async fn groups(&self) -> Result<Vec<CustomerGroup>> {
        Ok(
            sqlx::query_as::<_, CustomerGroup>("SELECT * FROM customer_groups ORDER BY code")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Create a customer group
    pub async fn create_group(
        &self,
        code: &str,
        name: &str,
        prices_exclude_tax: bool,
    ) -> Result<CustomerGroup> {
        let code = code.trim().to_ascii_lowercase();
        let valid = (1..=50).contains(&code.len())
            && code
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid || name.trim().is_empty() || name.chars().count() > 100 {
            return Err(Error::validation(
                "code (letters, digits, - and _) and name (at most 100 characters) are required",
            ));
        }
        sqlx::query_as::<_, CustomerGroup>(
            "INSERT INTO customer_groups (code, name, prices_exclude_tax) VALUES ($1, $2, $3) \
             ON CONFLICT (code) DO NOTHING RETURNING *",
        )
        .bind(&code)
        .bind(name.trim())
        .bind(prices_exclude_tax)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| conflict("A customer group with this code exists"))
    }

    /// What a customer's prices depend on: their active company and its
    /// group, else their own group
    pub async fn pricing_context(&self, customer: Uuid) -> Result<PricingContext> {
        let mut conn = self.pool.acquire().await?;
        crate::services::price_list_service::pricing_context(&mut conn, customer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn belgian_numbers_need_the_right_check_digits() {
        assert!(belgian_check_digits_ok("0123456749"));
        assert!(!belgian_check_digits_ok("0123456748"));
        assert!(!belgian_check_digits_ok("012345674"));
        let vat = parse_vat_number("be 123.456.749").unwrap();
        assert_eq!(vat.full_id(), "BE0123456749");
        assert!(parse_vat_number("BE0123456748").is_err());
        assert!(parse_vat_number("XX123").is_err());
        assert_eq!(
            parse_vat_number("FR40303265045").unwrap().country_code,
            "FR"
        );
    }
}
