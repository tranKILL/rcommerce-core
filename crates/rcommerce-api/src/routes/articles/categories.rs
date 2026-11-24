//! Article categories: a kind (`news` or `wiki`), a slug, a position, and a
//! name per language.

use std::collections::BTreeMap;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rcommerce_core::common::locale::normalize_locale;
use rcommerce_core::services::articles::Category;
use rcommerce_core::Error;
use serde::Deserialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{check_category, locales};
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct KindParams {
    pub kind: Option<String>,
    pub lang: Option<String>,
}

/// `GET /article-categories`: each with its name in the language asked for
/// (else the store's, else any)
pub async fn public_list(
    State(state): State<AppState>,
    Query(params): Query<KindParams>,
) -> Result<Json<serde_json::Value>, Error> {
    let (locale, fallback) = locales(&state, params.lang.as_deref());
    let rows: Vec<(Uuid, String, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT c.id, c.kind, c.slug, c.position, n.name FROM article_categories c \
         LEFT JOIN LATERAL (SELECT name FROM article_category_translations y \
           WHERE y.category_id = c.id \
           ORDER BY (y.locale = $2) DESC, (y.locale = $3) DESC, y.locale LIMIT 1) n ON true \
         WHERE ($1::text IS NULL OR c.kind = $1) ORDER BY c.kind, c.position, c.slug",
    )
    .bind(params.kind.as_deref())
    .bind(&locale)
    .bind(&fallback)
    .fetch_all(state.db.pool())
    .await?;
    let data: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(id, kind, slug, position, name)| {
            serde_json::json!({"id": id, "kind": kind, "slug": slug, "position": position,
                               "name": name.unwrap_or_else(|| slug.clone())})
        })
        .collect();
    Ok(Json(serde_json::json!({ "data": data })))
}

/// `GET /admin/article-categories`: every language's name
pub async fn admin_list(State(state): State<AppState>) -> Result<Json<Vec<Category>>, Error> {
    let rows: Vec<(Uuid, String, String, i32)> = sqlx::query_as(
        "SELECT id, kind, slug, position FROM article_categories ORDER BY kind, position, slug",
    )
    .fetch_all(state.db.pool())
    .await?;
    let names: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT category_id, locale, name FROM article_category_translations")
            .fetch_all(state.db.pool())
            .await?;
    let mut by_id: BTreeMap<Uuid, BTreeMap<String, String>> = BTreeMap::new();
    for (id, locale, name) in names {
        by_id.entry(id).or_default().insert(locale, name);
    }
    Ok(Json(
        rows.into_iter()
            .map(|(id, kind, slug, position)| Category {
                id: Some(id),
                kind,
                slug,
                position,
                names: by_id.remove(&id).unwrap_or_default(),
            })
            .collect(),
    ))
}

fn slug_taken(e: sqlx::Error) -> Error {
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Error::validation("Another category of this kind already uses this slug")
        }
        _ => e.into(),
    }
}

async fn write_names(
    conn: &mut PgConnection,
    id: Uuid,
    names: &BTreeMap<String, String>,
) -> Result<(), Error> {
    sqlx::query("DELETE FROM article_category_translations WHERE category_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for (locale, name) in names.iter().filter(|(_, n)| !n.trim().is_empty()) {
        let locale = normalize_locale(locale)
            .ok_or_else(|| Error::validation(format!("unknown language {locale}")))?;
        sqlx::query(
            "INSERT INTO article_category_translations (category_id, locale, name) \
             VALUES ($1, $2, $3) ON CONFLICT (category_id, locale) DO UPDATE SET name = $3",
        )
        .bind(id)
        .bind(locale)
        .bind(name.trim())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `POST /admin/article-categories`
pub async fn create(
    State(state): State<AppState>,
    Json(category): Json<Category>,
) -> Result<(StatusCode, Json<Category>), Error> {
    check_category(&category)?;
    let mut tx = state.db.pool().begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO article_categories (kind, slug, position) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&category.kind)
    .bind(&category.slug)
    .bind(category.position)
    .fetch_one(&mut *tx)
    .await
    .map_err(slug_taken)?;
    write_names(&mut tx, id, &category.names).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Category {
            id: Some(id),
            ..category
        }),
    ))
}

/// `PUT /admin/article-categories/:id` (its kind stays)
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(category): Json<Category>,
) -> Result<Json<Category>, Error> {
    check_category(&category)?;
    let mut tx = state.db.pool().begin().await?;
    let kind: Option<String> = sqlx::query_scalar(
        "UPDATE article_categories SET slug = $2, position = $3 WHERE id = $1 RETURNING kind",
    )
    .bind(id)
    .bind(&category.slug)
    .bind(category.position)
    .fetch_optional(&mut *tx)
    .await
    .map_err(slug_taken)?;
    let kind = kind.ok_or_else(|| Error::not_found("Category not found"))?;
    write_names(&mut tx, id, &category.names).await?;
    tx.commit().await?;
    Ok(Json(Category {
        id: Some(id),
        kind,
        ..category
    }))
}

/// `DELETE /admin/article-categories/:id`: its articles keep going, uncategorised
pub async fn remove(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    let done = sqlx::query("DELETE FROM article_categories WHERE id = $1")
        .bind(id)
        .execute(state.db.pool())
        .await?;
    if done.rows_affected() > 0 {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::not_found("Category not found"))
    }
}
