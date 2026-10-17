use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

use super::files::FileResponseQuery;
use crate::api::error::anonymous_not_found;
use crate::core::state::AppState;
use crate::credential::share::models::ShareKind;

pub fn router() -> Router<AppState> {
    Router::new().route("/s/{id}", get(resolve_share))
}

async fn resolve_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<FileResponseQuery>,
) -> Response {
    // "Not found" and "expired" return the SAME response so the route is
    // not an oracle for "does this id exist".
    let row = match state.share_service.resolve(&id).await {
        Ok(Some(row)) => row,
        Ok(None) => return anonymous_not_found(),
        Err(e) => {
            tracing::warn!(id = %id, error = %e, "share resolve failed");
            return anonymous_not_found();
        }
    };

    match row.kind {
        ShareKind::Chat { chat_id } => {
            // Relative redirect — the user is already on this host via `/s/{id}`.
            Redirect::to(&format!("/chats/{chat_id}")).into_response()
        }
        ShareKind::File {
            owner,
            path,
            public,
        } => {
            let mut target = if public {
                match state
                    .presign_service
                    .sign_by_user_id(&owner, &path, &row.user_id)
                    .await
                {
                    Ok(url) if !url.is_empty() => url,
                    Ok(_) => {
                        tracing::warn!(id = %id, "presign returned empty URL");
                        return anonymous_not_found();
                    }
                    Err(e) => {
                        tracing::warn!(id = %id, error = %e, "presign mint failed");
                        return anonymous_not_found();
                    }
                }
            } else {
                let user_handle = match state.user_service.handle_of(&row.user_id).await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(id = %id, error = %e, "handle lookup failed");
                        return anonymous_not_found();
                    }
                };
                let segment = match crate::storage::attachment_url_segment(
                    &owner,
                    &path,
                    Some(user_handle.as_ref()),
                ) {
                    Some(s) => s,
                    None => return anonymous_not_found(),
                };
                format!("/api/files/{segment}")
            };

            // Forward presentation without changing the share's authorization or
            // the exact file claim in its presign. Private shares still need auth.
            if query.download {
                target.push_str(if public {
                    "&download=true"
                } else {
                    "?download=true"
                });
            }

            // `Redirect::to` is 303 — the standard URL-shortener redirect
            // for GET. One-time hop semantics, no "the resource may
            // temporarily live here" implication.
            Redirect::to(&target).into_response()
        }
    }
}
