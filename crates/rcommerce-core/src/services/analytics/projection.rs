//! Sales projection for the coming months, from monthly history.
//!
//! When last year is known, a month is projected as the same month last year
//! times the trailing ratio: the last three complete months against the same
//! three months a year earlier (kept between ×0.25 and ×4). Without last year,
//! a straight line is fitted through up to the last 12 complete months. With
//! fewer than three complete months there is no projection.

use std::collections::BTreeMap;

use chrono::{Months, NaiveDate};
use rust_decimal::Decimal;
use serde::Serialize;

/// How a month was projected
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// The same month last year times the trailing year-on-year ratio
    LastYear,
    /// A straight line through the last complete months
    Trend,
}

/// One projected month
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectedMonth {
    /// First day of the month
    pub month: NaiveDate,
    /// Expected sales excl. VAT
    pub sales: Decimal,
    pub method: Method,
}

/// Monthly sales of complete months, with the months between the first sale
/// and the last complete month filled in as zero
struct History {
    months: Vec<(NaiveDate, Decimal)>,
}

impl History {
    fn new(sales: &BTreeMap<NaiveDate, Decimal>, current: NaiveDate) -> Self {
        let mut months = Vec::new();
        if let Some((&first, _)) = sales.iter().find(|(m, _)| **m < current) {
            let mut month = first;
            while month < current {
                months.push((month, sales.get(&month).copied().unwrap_or_default()));
                month = month + Months::new(1);
            }
        }
        Self { months }
    }

    fn get(&self, month: NaiveDate) -> Option<Decimal> {
        self.months
            .iter()
            .find(|(m, _)| *m == month)
            .map(|(_, sales)| *sales)
    }

    /// The last three complete months against the same months a year earlier
    fn ratio(&self) -> Option<Decimal> {
        let n = self.months.len();
        if n < 15 {
            return None;
        }
        let recent: Decimal = self.months[n - 3..].iter().map(|(_, s)| *s).sum();
        let before: Decimal = self.months[n - 15..n - 12].iter().map(|(_, s)| *s).sum();
        if before <= Decimal::ZERO {
            return None;
        }
        let limit = |r: Decimal| r.max(Decimal::new(25, 2)).min(Decimal::from(4));
        Some(limit(recent / before))
    }

    /// Intercept and slope of the line through the last 12 complete months
    /// (x = 0 for the oldest of them)
    fn trend(&self) -> Option<(Decimal, Decimal, usize)> {
        let points = &self.months[self.months.len().saturating_sub(12)..];
        let n = points.len();
        if n < 3 {
            return None;
        }
        let count = Decimal::from(n as u64);
        let (mut sx, mut sy, mut sxy, mut sxx) =
            (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
        for (x, (_, y)) in points.iter().enumerate() {
            let x = Decimal::from(x as u64);
            sx += x;
            sy += *y;
            sxy += x * *y;
            sxx += x * x;
        }
        let slope = (count * sxy - sx * sy) / (count * sxx - sx * sx);
        let intercept = (sy - slope * sx) / count;
        Some((intercept, slope, n))
    }
}

/// The current month (`current`, a first day) and the `horizon` months after
/// it, projected from the sales of complete months (`sales`, keyed by first
/// day; months after `current` are ignored)
pub fn project(
    sales: &BTreeMap<NaiveDate, Decimal>,
    current: NaiveDate,
    horizon: u32,
) -> Vec<ProjectedMonth> {
    let history = History::new(sales, current);
    let ratio = history.ratio();
    let trend = history.trend();
    (0..=horizon)
        .filter_map(|ahead| {
            let month = current + Months::new(ahead);
            let last_year = history.get(month - Months::new(12));
            let (sales, method) = match (ratio, last_year, trend) {
                (Some(ratio), Some(before), _) => (before * ratio, Method::LastYear),
                (_, _, Some((intercept, slope, n))) => {
                    let x = Decimal::from(n as u64 + u64::from(ahead));
                    (intercept + slope * x, Method::Trend)
                }
                _ => return None,
            };
            Some(ProjectedMonth {
                month,
                sales: sales.max(Decimal::ZERO).round_dp(2),
                method,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn month(s: &str) -> NaiveDate {
        format!("{s}-01").parse().unwrap()
    }

    fn series(start: &str, values: &[i64]) -> BTreeMap<NaiveDate, Decimal> {
        let first = month(start);
        values
            .iter()
            .enumerate()
            .map(|(i, v)| (first + Months::new(i as u32), Decimal::from(*v)))
            .collect()
    }

    #[test]
    fn a_known_year_projects_last_year_times_the_trailing_ratio() {
        // Nov 2024 - Oct 2025 at 100 a month except a December peak, then the
        // same months of 2025-26 10 % higher
        let mut values = vec![100, 300, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100];
        values.extend(values.clone().iter().map(|v| v * 11 / 10));
        let sales = series("2024-11", &values);
        let projected = project(&sales, month("2026-11"), 2);
        assert_eq!(projected.len(), 3);
        assert_eq!(projected[0].method, Method::LastYear);
        assert_eq!(projected[0].sales, Decimal::from(110) * Decimal::new(11, 1));
        // December again: last December (330) × 1.1
        assert_eq!(projected[1].month, month("2026-12"));
        assert_eq!(projected[1].sales, Decimal::from(363));
    }

    #[test]
    fn without_last_year_a_straight_line_is_followed() {
        let sales = series("2026-05", &[100, 110, 120, 130, 140, 150]);
        let projected = project(&sales, month("2026-11"), 1);
        assert_eq!(projected[0].method, Method::Trend);
        assert_eq!(projected[0].sales, Decimal::from(160));
        assert_eq!(projected[1].sales, Decimal::from(170));
    }

    #[test]
    fn months_without_sales_count_as_zero_and_projections_stay_positive() {
        // Falling sales with a month missing in the middle (zero): the line
        // drops below zero, which shows as zero
        let mut sales = series("2026-07", &[300, 200]);
        sales.insert(month("2026-10"), Decimal::ZERO);
        let projected = project(&sales, month("2026-11"), 3);
        assert_eq!(projected.len(), 4);
        assert!(projected.iter().all(|p| p.sales >= Decimal::ZERO));
        assert_eq!(projected[3].sales, Decimal::ZERO);
    }

    #[test]
    fn too_little_history_projects_nothing() {
        let sales = series("2026-09", &[100, 120]);
        assert!(project(&sales, month("2026-11"), 3).is_empty());
        // The current month's own sales are not history yet
        let sales = series("2026-09", &[100, 120, 500]);
        assert!(project(&sales, month("2026-11"), 3).is_empty());
    }

    #[test]
    fn the_ratio_is_kept_within_reason() {
        // A year that went from almost nothing to normal: ×4 at most
        let mut values = vec![1; 12];
        values.extend([100; 12]);
        let sales = series("2024-11", &values);
        let projected = project(&sales, month("2026-11"), 0);
        assert_eq!(projected[0].sales, Decimal::from(400));
    }
}
