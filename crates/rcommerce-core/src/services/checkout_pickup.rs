//! Collecting an order at a store location ("click and collect").
//!
//! Every active, public pickup location is offered in checkout as a free
//! shipping rate, ready at the location's first opening after its preparation
//! time. Choosing that rate reserves the order's stock at the location.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::location::StoreLocation;
use crate::repository::LocationRepository;
use crate::services::location_service::OpeningSchedule;
use crate::shipping::ShippingRate;
use crate::Result;

/// `provider_id` and `carrier` of pickup rates; the `service_code` is the location id
pub const PICKUP_PROVIDER: &str = "pickup";

/// Special days loaded ahead of now, enough to find the next opening after a
/// long holiday closure
const SPECIAL_DAYS_AHEAD: i64 = 31;

/// The store's pickup locations as checkout options
pub struct PickupLocations {
    repo: LocationRepository,
}

impl PickupLocations {
    /// Pickup options read from the store's locations
    pub fn new(pool: PgPool) -> Self {
        Self {
            repo: LocationRepository::new(pool),
        }
    }

    /// One free rate per active, public pickup location, in display order,
    /// with its ready time for an order placed at `now`
    pub async fn rates(&self, currency: &str, now: DateTime<Utc>) -> Result<Vec<ShippingRate>> {
        let locations: Vec<StoreLocation> = self
            .repo
            .list(true, None)
            .await?
            .into_iter()
            .filter(|l| l.is_pickup_point)
            .collect();
        if locations.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<Uuid> = locations.iter().map(|l| l.id).collect();
        let from = (now - Duration::days(1)).date_naive();
        let to = (now + Duration::days(SPECIAL_DAYS_AHEAD)).date_naive();
        let (mut hours, mut closures) = tokio::try_join!(
            self.repo.opening_hours(&ids),
            self.repo.closures(&ids, from, to)
        )?;
        let ready: HashMap<Uuid, Option<DateTime<Utc>>> = locations
            .iter()
            .map(|l| {
                let schedule = OpeningSchedule::new(
                    &l.timezone,
                    hours.remove(&l.id).unwrap_or_default(),
                    closures.remove(&l.id).unwrap_or_default(),
                );
                (l.id, schedule.pickup_ready_at(now, l.pickup_lead_minutes))
            })
            .collect();
        Ok(locations
            .iter()
            .map(|l| pickup_rate(l, ready.get(&l.id).copied().flatten(), currency))
            .collect())
    }

    /// Whether stock can be reserved at a location: it exists and is active
    pub async fn is_active(&self, id: Uuid) -> Result<bool> {
        Ok(self
            .repo
            .get(&id.to_string(), false, None)
            .await?
            .is_some_and(|l| l.is_active))
    }
}

/// The free rate for collecting at `location`, ready at `ready_at` (none when
/// the location has no opening hours)
pub fn pickup_rate(
    location: &StoreLocation,
    ready_at: Option<DateTime<Utc>>,
    currency: &str,
) -> ShippingRate {
    let mut rate = ShippingRate::new(
        PICKUP_PROVIDER,
        PICKUP_PROVIDER,
        location.id.to_string(),
        location.name.clone(),
        Decimal::ZERO,
        currency,
    );
    rate.delivery_date = ready_at;
    rate.estimated = false;
    rate
}

/// The location a pickup rate stands for (`None` for any other rate)
pub fn pickup_location(rate: &ShippingRate) -> Option<Uuid> {
    (rate.provider_id == PICKUP_PROVIDER && rate.carrier == PICKUP_PROVIDER)
        .then(|| Uuid::parse_str(&rate.service_code).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(name: &str) -> StoreLocation {
        StoreLocation {
            id: Uuid::new_v4(),
            name: name.to_string(),
            code: "NTH".to_string(),
            slug: Some("north".to_string()),
            is_active: true,
            is_public: true,
            is_pickup_point: true,
            phone: None,
            email: None,
            address_line1: None,
            address_line2: None,
            postal_code: None,
            city: None,
            country_code: None,
            latitude: None,
            longitude: None,
            timezone: "Europe/Brussels".to_string(),
            pickup_lead_minutes: 60,
            services: vec!["pickup".to_string()],
            manager_name: None,
            sort_order: 0,
            description: None,
            pickup_instructions: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_pickup_rate_is_free_and_names_its_location() {
        let branch = location("North branch");
        let ready = Utc::now() + Duration::hours(1);
        let rate = pickup_rate(&branch, Some(ready), "EUR");
        assert_eq!(rate.total_cost, Decimal::ZERO);
        assert_eq!(rate.service_name, "North branch");
        assert_eq!(rate.delivery_date, Some(ready));
        assert_eq!(pickup_location(&rate), Some(branch.id));
    }

    #[test]
    fn other_rates_have_no_pickup_location() {
        let carrier = ShippingRate::new(
            "weight_based",
            "Store",
            "standard",
            "Standard",
            Decimal::ONE,
            "EUR",
        );
        assert_eq!(pickup_location(&carrier), None);
        // A carrier rate whose code happens to be a uuid is still not a pickup
        let mut spoofed = carrier.clone();
        spoofed.service_code = Uuid::new_v4().to_string();
        assert_eq!(pickup_location(&spoofed), None);
        let mut bad_code = pickup_rate(&location("North"), None, "EUR");
        bad_code.service_code = "north".to_string();
        assert_eq!(pickup_location(&bad_code), None);
    }
}
