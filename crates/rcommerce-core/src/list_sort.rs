//! Sorting of list endpoints. A `sort` parameter names a column, with `-` in
//! front for descending order (`sort=-total`). Each list declares the columns
//! it can be sorted on as fixed SQL expressions, so request text never reaches
//! the SQL; an unknown column is a validation error.

use crate::{Error, Result};

/// The columns of a list: the name used in `sort`, and its SQL expression
pub type SortColumns = &'static [(&'static str, &'static str)];

/// The `ORDER BY` terms (without the keywords) for `param`, or for `default`
/// when it is missing or empty. Missing values sort last in both directions;
/// `tie_break` (a unique column) keeps pages stable among equal values.
pub fn order_by(
    param: Option<&str>,
    columns: SortColumns,
    default: &str,
    tie_break: &str,
) -> Result<String> {
    let wanted = param
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(default);
    let (descending, name) = match wanted.strip_prefix('-') {
        Some(name) => (true, name),
        None => (false, wanted),
    };
    let Some((_, expression)) = columns.iter().find(|(column, _)| *column == name) else {
        let known: Vec<&str> = columns.iter().map(|(column, _)| *column).collect();
        return Err(Error::validation(format!(
            "Cannot sort by '{name}': use one of {}, with '-' in front for descending order",
            known.join(", ")
        )));
    };
    let direction = if descending { "DESC" } else { "ASC" };
    Ok(format!("{expression} {direction} NULLS LAST, {tie_break}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLUMNS: SortColumns = &[("name", "lower(name)"), ("total", "total")];

    #[test]
    fn a_column_sorts_up_and_a_dash_sorts_down() {
        assert_eq!(
            order_by(Some("name"), COLUMNS, "-total", "id").unwrap(),
            "lower(name) ASC NULLS LAST, id"
        );
        assert_eq!(
            order_by(Some("-total"), COLUMNS, "name", "id").unwrap(),
            "total DESC NULLS LAST, id"
        );
    }

    #[test]
    fn no_sort_uses_the_default() {
        assert_eq!(
            order_by(None, COLUMNS, "-total", "id").unwrap(),
            "total DESC NULLS LAST, id"
        );
        assert_eq!(
            order_by(Some(" "), COLUMNS, "name", "id").unwrap(),
            "lower(name) ASC NULLS LAST, id"
        );
    }

    #[test]
    fn unknown_columns_are_refused_without_reaching_the_sql() {
        let err = order_by(Some("total; DROP TABLE x"), COLUMNS, "name", "id").unwrap_err();
        assert!(err.to_string().contains("name, total"), "{err}");
    }
}
