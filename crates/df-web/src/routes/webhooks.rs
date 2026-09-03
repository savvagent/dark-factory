use std::str::FromStr;

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::Json;
use df_core::trackers::{
    find_binding_by_external_ref, get_connection, resolve_connection_org, Provider,
};
use df_trackers::webhook::{jira_site_id, verify_and_parse, ParsedWebhook, Verification};
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

fn webhook_not_found() -> ApiError {
    ApiError::not_found("no matching webhook")
}

pub async fn receive(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let provider = Provider::from_str(&provider).map_err(|error| {
        tracing::warn!(error = %error, "webhook request named an unknown provider");
        webhook_not_found()
    })?;

    let raw_body = body.as_ref();
    let (parsed, org_id) = match provider {
        Provider::Github => {
            let secret = state
                .config
                .github_app_webhook_secret
                .as_deref()
                .ok_or_else(|| {
                    ApiError::internal(
                        "verify GitHub webhook",
                        "DF_GITHUB_APP_WEBHOOK_SECRET is not configured",
                    )
                })?;
            let parsed = verify_and_parse(
                provider,
                &headers,
                query.as_deref(),
                raw_body,
                Verification::Github { secret },
            )
            .map_err(|error| {
                tracing::warn!(provider = %provider, error = %error, "webhook rejected");
                webhook_not_found()
            })?;
            let external_id = parsed.connection_external_id().to_string();
            let org_id = resolve_connection_org(&state.db, provider, &external_id)
                .await
                .map_err(ApiError::from)?;
            (parsed, org_id)
        }
        Provider::Jira => {
            let site_id = jira_site_id(query.as_deref()).map_err(|error| {
                tracing::warn!(provider = %provider, error = %error, "webhook rejected");
                webhook_not_found()
            })?;
            let org_id = resolve_connection_org(&state.db, provider, &site_id)
                .await
                .map_err(ApiError::from)?;
            let Some(org_id) = org_id else {
                tracing::warn!(provider = %provider, external_id = %site_id, "webhook did not match a registered connection");
                return Err(webhook_not_found());
            };

            let mut tx = state.db.begin(org_id).await.map_err(ApiError::from)?;
            let connection = get_connection(&mut tx, provider)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(|| {
                    ApiError::internal(
                        "load resolved JIRA connection",
                        format!(
                            "tracker_connection_index resolved org {org_id}, but org has no jira connection"
                        ),
                    )
                })?;
            let encoded_secret = connection
                .encrypted_webhook_secret
                .as_deref()
                .ok_or_else(|| {
                    tracing::warn!(provider = %provider, org_id = %org_id, "webhook secret missing for resolved connection");
                    webhook_not_found()
                })?;
            let parsed = verify_and_parse(
                provider,
                &headers,
                query.as_deref(),
                raw_body,
                Verification::Jira {
                    cipher: &state.cipher,
                    encoded_secret,
                },
            )
            .map_err(|error| {
                tracing::warn!(
                    provider = %provider,
                    org_id = %org_id,
                    error = %error,
                    "webhook rejected"
                );
                webhook_not_found()
            })?;
            drop(tx);
            (parsed, Some(org_id))
        }
    };

    let Some(org_id) = org_id else {
        tracing::warn!(
            provider = %provider,
            external_id = %parsed.connection_external_id(),
            "webhook did not match a registered connection"
        );
        return Err(webhook_not_found());
    };

    let mut tx = state.db.begin(org_id).await.map_err(ApiError::from)?;
    let connection = get_connection(&mut tx, provider)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::internal(
                "load resolved tracker connection",
                format!(
                    "tracker_connection_index resolved org {org_id}, but org has no {provider} connection"
                ),
            )
        })?;

    match &parsed {
        ParsedWebhook::Event(event) => {
            let binding =
                find_binding_by_external_ref(&mut tx, provider, &event.binding_external_ref)
                    .await
                    .map_err(ApiError::from)?;

            tracing::info!(
                provider = %event.provider,
                org_id = %org_id,
                connection_id = %connection.id,
                external_id = %event.connection_external_id,
                external_ref = %event.binding_external_ref,
                action = %event.action,
                issue_ref = %event.issue.reference,
                binding_id = binding.as_ref().map(|binding| binding.id.to_string()),
                "webhook verified and parsed"
            );

            if binding.is_none() {
                tracing::warn!(
                    provider = %event.provider,
                    org_id = %org_id,
                    external_ref = %event.binding_external_ref,
                    "webhook verified for a connection with no matching repo binding"
                );
            }

            // Task 4 turns a verified tracker event into job-sync work. Task 3
            // stops at verification, org resolution, and binding lookup so the
            // inbound surface is correct before it starts mutating queue state.
        }
        ParsedWebhook::Ignored(ignored) => {
            tracing::info!(
                provider = %ignored.provider,
                org_id = %org_id,
                connection_id = %connection.id,
                external_id = %ignored.connection_external_id,
                event = %ignored.event,
                "webhook verified and ignored"
            );
        }
    }

    tx.commit().await.map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true })))
}
