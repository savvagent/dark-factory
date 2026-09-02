//! The OpenAPI 3.1 document, rendered from [`crate::catalog`].
//!
//! The same list the router is built from, read a second way. A route that
//! exists is described, because describing it and mounting it are the same
//! declaration — see the catalog's module docs for why that is the shape chosen.
//!
//! Component schemas are hand-written here rather than derived from the Rust
//! types. That keeps OpenAPI's vocabulary out of `df-core`, which describes
//! itself to MCP clients through `schemars` already and has no business
//! carrying a second documentation dependency for a surface it does not serve.

use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::catalog::{catalog, Auth, Endpoint};
use crate::state::AppState;

/// `GET /api/openapi.json`.
pub async fn serve(State(_state): State<AppState>) -> Json<Value> {
    static DOC: OnceLock<Arc<Value>> = OnceLock::new();
    let doc = DOC.get_or_init(|| Arc::new(document(&catalog())));
    Json((**doc).clone())
}

pub fn document(endpoints: &[Endpoint]) -> Value {
    let mut paths = serde_json::Map::new();

    for endpoint in endpoints {
        let entry = paths
            .entry(endpoint.path.to_string())
            .or_insert_with(|| json!({}));

        let mut operation = json!({
            "operationId": endpoint.operation_id(),
            "summary": endpoint.summary,
            "description": endpoint.description,
            "tags": [tag_for(endpoint.path)],
            "responses": responses(endpoint),
        });

        let params = endpoint
            .path_params()
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "in": "path",
                    "required": true,
                    "description": param_description(name),
                    "schema": { "type": "string" },
                })
            })
            .collect::<Vec<_>>();

        if !params.is_empty() {
            operation["parameters"] = json!(params);
        }

        if let Some(schema) = endpoint.request {
            operation["requestBody"] = json!({
                "required": true,
                "content": { "application/json": { "schema": reference(schema) } },
            });
        }

        if endpoint.auth != Auth::Public {
            operation["security"] = json!([{ "sessionCookie": [] }]);
        }
        operation["x-dark-factory-auth"] = json!(endpoint.auth.as_str());

        entry[endpoint.verb.as_str()] = operation;
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "dark-factory console API",
            "version": env!("CARGO_PKG_VERSION"),
            "description":
                "The console's REST surface, and the OAuth 2.1 authorization server \
                 in front of the MCP endpoint. Authentication is a session cookie \
                 (`__Host-df_session`), set by the sign-in endpoints; the MCP surface \
                 itself uses bearer tokens and is described by its own metadata \
                 documents.",
        },
        "components": {
            "securitySchemes": {
                "sessionCookie": {
                    "type": "apiKey",
                    "in": "cookie",
                    "name": crate::session::COOKIE_NAME,
                    "description":
                        "Set on sign-in. HttpOnly, Secure, SameSite=Lax — browsers \
                         send it automatically; it cannot be read from script.",
                },
            },
            "schemas": components(),
        },
        "paths": paths,
    })
}

fn reference(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// The response block. Every endpoint can fail the same four ways, and saying
/// so once here is what makes the document usable without reading the source.
fn responses(endpoint: &Endpoint) -> Value {
    let success = match (endpoint.verb, endpoint.response) {
        (_, Some(schema)) => json!({
            "description": "Success",
            "content": { "application/json": { "schema": reference(schema) } },
        }),
        (crate::catalog::Verb::Delete, None) | (_, None) => json!({ "description": "Success" }),
    };

    let error = json!({
        "description": "Failure",
        "content": { "application/json": { "schema": reference("Error") } },
    });

    let mut responses = json!({
        "200": success,
        "400": error,
        "500": error,
    });

    if endpoint.auth != Auth::Public {
        responses["401"] = error.clone();
    }
    if endpoint.auth.needs_org() {
        // 404 rather than 403 for an org you are not in — see `OrgCtx`.
        responses["403"] = error.clone();
        responses["404"] = error;
    }

    responses
}

fn param_description(name: &str) -> &'static str {
    match name {
        "org" => "Org slug.",
        "team" => "Team slug.",
        "repo" => "Repo slug — the handle agents use.",
        "user" => "User id (UUID).",
        "id" => "Resource id (UUID).",
        _ => "",
    }
}

fn tag_for(path: &str) -> &'static str {
    if path.starts_with("/oauth") || path.starts_with("/.well-known") {
        "oauth"
    } else if path.starts_with("/api/auth") {
        "auth"
    } else if path.starts_with("/api/me") {
        "me"
    } else if path.contains("/teams") {
        "teams"
    } else if path.contains("/repos") {
        "repos"
    } else if path.contains("/tokens") {
        "tokens"
    } else if path.contains("/invites") {
        "invites"
    } else if path.contains("/usage") || path.contains("/audit") {
        "billing"
    } else {
        "orgs"
    }
}

/// The component schemas the catalog refers to by name.
///
/// Split across three functions because `serde_json::json!` is recursive and one
/// literal this size exhausts the macro recursion limit — a compile error, not a
/// runtime one, but a confusing enough compile error to be worth avoiding.
fn components() -> Value {
    let mut all = serde_json::Map::new();
    for group in [
        entity_schemas(),
        queue_schemas(),
        response_schemas(),
        request_schemas(),
    ] {
        let Value::Object(map) = group else {
            unreachable!("each schema group is an object literal")
        };
        all.extend(map);
    }
    Value::Object(all)
}

/// The domain objects — what `df-core` returns, as the wire sees it.
fn entity_schemas() -> Value {
    let timestamp = json!({ "type": "string", "format": "date-time" });
    let uuid = json!({ "type": "string", "format": "uuid" });
    let role = json!({ "type": "string", "enum": ["owner", "admin", "member"] });

    let user = json!({
        "type": "object",
        "properties": {
            "id": uuid,
            "email": { "type": "string", "format": "email" },
            "name": { "type": ["string", "null"] },
            "emailVerifiedAt": { "type": ["string", "null"], "format": "date-time" },
            "createdAt": timestamp,
            "disabledAt": { "type": ["string", "null"], "format": "date-time" },
        },
        "required": ["id", "email", "createdAt"],
    });

    let org = json!({
        "type": "object",
        "properties": {
            "id": uuid,
            "slug": { "type": "string" },
            "name": { "type": "string" },
            "plan": { "type": "string", "enum": ["free", "team", "business", "enterprise"] },
            "enforceSso": { "type": "boolean" },
            "createdAt": timestamp,
        },
        "required": ["id", "slug", "name", "plan"],
    });

    let membership = json!({
        "type": "object",
        "description": "One org this account belongs to, and the role it holds there.",
        "properties": {
            "orgId": uuid,
            "userId": uuid,
            "role": role,
            "orgSlug": { "type": "string" },
            "orgName": { "type": "string" },
            "plan": { "type": "string" },
        },
        "required": ["orgId", "userId", "role", "orgSlug", "orgName"],
    });

    let org_member = json!({
        "type": "object",
        "properties": {
            "id": uuid,
            "email": { "type": "string" },
            "name": { "type": ["string", "null"] },
            "role": role,
            "joinedAt": timestamp,
            "emailVerifiedAt": { "type": ["string", "null"], "format": "date-time" },
            "disabledAt": { "type": ["string", "null"], "format": "date-time" },
        },
        "required": ["id", "email", "role", "joinedAt"],
    });

    let team = json!({
        "type": "object",
        "properties": {
            "id": uuid,
            "orgId": uuid,
            "slug": { "type": "string" },
            "name": { "type": "string" },
            "createdAt": timestamp,
        },
        "required": ["id", "orgId", "slug", "name"],
    });

    let repo = json!({
        "type": "object",
        "properties": {
            "id": uuid,
            "orgId": uuid,
            "slug": { "type": "string", "description": "The handle agents use." },
            "name": { "type": "string" },
            "provider": { "type": "string", "enum": ["github", "gitlab", "bitbucket", "other"] },
            "defaultBranch": { "type": "string" },
            "teamId": { "type": ["string", "null"], "format": "uuid" },
            "defaultAgentType": { "type": ["string", "null"] },
            "trackerBinding": { "type": "object" },
            "active": { "type": "boolean" },
            "createdAt": timestamp,
            "createdBy": { "type": ["string", "null"], "format": "uuid" },
        },
        "required": ["id", "orgId", "slug", "name", "provider", "defaultBranch", "active"],
    });

    let invite = json!({
        "type": "object",
        "properties": {
            "id": uuid,
            "orgId": uuid,
            "email": { "type": "string" },
            "role": role,
            "invitedBy": { "type": ["string", "null"], "format": "uuid" },
            "expiresAt": timestamp,
            "acceptedAt": { "type": ["string", "null"], "format": "date-time" },
            "createdAt": timestamp,
        },
        "required": ["id", "orgId", "email", "role", "expiresAt"],
    });

    json!({
        "Error": {
            "type": "object",
            "description":
                "Every failure. `code` is stable and safe to branch on; `message` is \
                 written to be shown to a person.",
            "properties": {
                "error": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "string", "examples": ["invalid_credentials"] },
                        "message": { "type": "string" },
                    },
                    "required": ["code", "message"],
                },
            },
            "required": ["error"],
        },
        "User": user,
        "Org": org,
        "Membership": membership,
        "MembershipList": { "type": "array", "items": reference("Membership") },
        "OrgMember": org_member,
        "OrgMemberList": { "type": "array", "items": reference("OrgMember") },
        "Team": team,
        "TeamList": { "type": "array", "items": reference("Team") },
        "TeamMember": {
            "type": "object",
            "properties": {
                "userId": uuid,
                "email": { "type": "string" },
                "name": { "type": ["string", "null"] },
                "joinedAt": timestamp,
            },
            "required": ["userId", "email", "joinedAt"],
        },
        "TeamMemberList": { "type": "array", "items": reference("TeamMember") },
        "Repo": repo,
        "RepoList": { "type": "array", "items": reference("Repo") },
        "Invite": invite,
        "InviteList": { "type": "array", "items": reference("Invite") },
        "Lease": {
            "type": "object",
            "description":
                "An advisory, time-bounded claim on one branch of one repo. The server \
                 cannot enforce it against a git operation it cannot see; it makes \
                 collisions visible rather than impossible.",
            "properties": {
                "id": uuid,
                "repoId": uuid,
                "branch": { "type": "string" },
                "holderUserId": uuid,
                "holderLabel": { "type": ["string", "null"] },
                "jobId": { "type": ["string", "null"] },
                "acquiredAt": timestamp,
                "renewedAt": timestamp,
                "expiresAt": timestamp,
            },
            "required": ["id", "repoId", "branch", "holderUserId", "expiresAt"],
        },
        "LeaseList": { "type": "array", "items": reference("Lease") },
        "Session": {
            "type": "object",
            "properties": {
                "id": uuid,
                "userId": uuid,
                "expiresAt": timestamp,
                "createdAt": timestamp,
            },
            "required": ["id", "userId", "expiresAt", "createdAt"],
        },
        "SessionList": { "type": "array", "items": reference("Session") },
        "TokenSummary": {
            "type": "object",
            "description": "A live credential. Never includes the token itself.",
            "properties": {
                "id": uuid,
                "name": { "type": ["string", "null"] },
                "kind": { "type": "string", "enum": ["oauth", "pat"] },
                "clientId": { "type": ["string", "null"] },
                "scopes": { "type": "array", "items": { "type": "string" } },
                "createdAt": timestamp,
                "lastUsedAt": { "type": ["string", "null"], "format": "date-time" },
                "expiresAt": timestamp,
            },
            "required": ["id", "kind", "scopes", "createdAt", "expiresAt"],
        },
        "TokenSummaryList": { "type": "array", "items": reference("TokenSummary") },
        "MintedToken": {
            "type": "object",
            "description": "Shown once. Only a SHA-256 hash of `token` is stored.",
            "properties": {
                "token": { "type": "string", "examples": ["df_pat_…"] },
                "id": uuid,
                "name": { "type": "string" },
                "scopes": { "type": "array", "items": { "type": "string" } },
                "resource": {
                    "type": "string",
                    "description": "The MCP endpoint this token is audienced for.",
                },
            },
            "required": ["token", "id", "scopes", "resource"],
        },
        "UsageStatus": {
            "type": "object",
            "description":
                "This period's metered usage. `billableUsed` counts only billable \
                 tool calls; `totalCalls` counts every call, free ones included.",
            "properties": {
                "plan": { "type": "string" },
                "includedOps": { "type": "integer" },
                "billableUsed": { "type": "integer" },
                "remaining": { "type": "integer" },
                "totalCalls": { "type": "integer" },
                "periodStart": { "type": "string", "format": "date" },
                "warning": { "type": "boolean" },
                "hardStop": { "type": "boolean" },
                "enforced": { "type": "boolean" },
            },
            "required": ["plan", "includedOps", "billableUsed", "remaining"],
        },
        "AuditEvent": {
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "orgId": { "type": ["string", "null"], "format": "uuid" },
                "actorUserId": { "type": ["string", "null"], "format": "uuid" },
                "actorLabel": { "type": ["string", "null"] },
                "action": { "type": "string", "examples": ["org.member.invited"] },
                "targetType": { "type": ["string", "null"] },
                "targetId": { "type": ["string", "null"] },
                "ip": { "type": ["string", "null"] },
                "userAgent": { "type": ["string", "null"] },
                "detail": { "type": "object" },
                "createdAt": timestamp,
            },
            "required": ["id", "action", "createdAt"],
        },
        "AuditEventList": { "type": "array", "items": reference("AuditEvent") },
    })
}

/// The queue's schemas.
///
/// A separate group only because `serde_json::json!` expands recursively and
/// the entity literal had grown past the macro's recursion limit. Splitting is
/// the honest fix; raising `recursion_limit` crate-wide to keep one big literal
/// would hide the next one.
fn queue_schemas() -> Value {
    let timestamp = json!({ "type": "string", "format": "date-time" });
    let uuid = json!({ "type": "string", "format": "uuid" });

    json!({
        "Job": {
            "type": "object",
            "description":
                "One unit of coordinated work, always anchored to a repo. `metadata` \
                 is opaque — dark-factory never reads it; it is where a customer's own \
                 skill keeps whatever its methodology needs.",
            "properties": {
                "id": { "type": "string", "examples": ["job-42"] },
                "orgId": uuid,
                "repoId": uuid,
                "teamId": { "type": ["string", "null"], "format": "uuid" },
                "title": { "type": "string" },
                "description": { "type": ["string", "null"] },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in-progress", "completed", "failed"],
                },
                "ticketRef": { "type": ["string", "null"], "examples": ["ACME-17"] },
                "tracker": { "type": ["string", "null"], "enum": ["jira", "github", null] },
                "agentType": { "type": ["string", "null"] },
                "metadata": { "type": "object" },
                "createdAt": timestamp,
                "startedAt": { "type": ["string", "null"], "format": "date-time" },
                "completedAt": { "type": ["string", "null"], "format": "date-time" },
                "attempts": { "type": "integer" },
                "result": { "type": ["string", "null"] },
                "error": { "type": ["string", "null"] },
                "createdBy": { "type": ["string", "null"], "format": "uuid" },
                "claimedBy": { "type": ["string", "null"], "format": "uuid" },
                "claimedByLabel": { "type": ["string", "null"] },
            },
            "required": ["id", "orgId", "repoId", "title", "status", "createdAt", "attempts"],
        },
        "JobList": { "type": "array", "items": reference("Job") },
        "JobDetail": {
            "allOf": [
                reference("Job"),
                {
                    "type": "object",
                    "properties": {
                        "dependsOn": {
                            "type": "array",
                            "items": { "type": "string", "examples": ["job-41"] },
                            "description":
                                "Job ids that must reach `completed` before this one \
                                 is claimable.",
                        },
                    },
                    "required": ["dependsOn"],
                },
            ],
        },
        "QueueStats": {
            "type": "object",
            "description":
                "`blocked` overlaps `pending` rather than partitioning it: it counts \
                 the pending jobs still waiting on a dependency.",
            "properties": {
                "pending": { "type": "integer" },
                "inProgress": { "type": "integer" },
                "completed": { "type": "integer" },
                "failed": { "type": "integer" },
                "blocked": { "type": "integer" },
                "total": { "type": "integer" },
            },
            "required": ["pending", "inProgress", "completed", "failed", "blocked", "total"],
        },
    })
}

/// Wrappers the console reads back from an action.
fn response_schemas() -> Value {
    let role = json!({ "type": "string", "enum": ["owner", "admin", "member"] });

    json!({
        "Me": {
            "type": "object",
            "properties": {
                "user": reference("User"),
                "orgs": reference("MembershipList"),
                "mustEnrollTotp": { "type": "boolean" },
                "recoveryCodesRemaining": { "type": "integer" },
            },
            "required": ["user", "orgs", "mustEnrollTotp"],
        },
        "Joined": {
            "type": "object",
            "properties": { "org": reference("Org"), "role": role },
            "required": ["org", "role"],
        },
        "Accepted": {
            "type": "object",
            "description":
                "The constant-shape answer to every 'we have sent you something' \
                 request. Identical whether or not the address exists.",
            "properties": {
                "sent": { "type": "boolean" },
                "message": { "type": "string" },
            },
            "required": ["sent", "message"],
        },
        "Verified": {
            "type": "object",
            "properties": {
                "emailVerified": { "type": "boolean" },
                "mustEnrollTotp": { "type": "boolean" },
                "signedIn": {
                    "type": "boolean",
                    "description":
                        "Whether a session cookie came with this response. Only an \
                         account with no confirmed authenticator gets one.",
                },
            },
            "required": ["emailVerified", "mustEnrollTotp", "signedIn"],
        },
        "SessionOpened": {
            "type": "object",
            "properties": {
                "user": reference("User"),
                "mustEnrollTotp": { "type": "boolean" },
            },
            "required": ["user", "mustEnrollTotp"],
        },
        "Enrollment": {
            "type": "object",
            "description": "Shown exactly once. Only hashes are stored.",
            "properties": {
                "provisioningUri": {
                    "type": "string",
                    "description": "otpauth:// URI — render as a QR code.",
                },
                "manualKey": { "type": "string" },
                "recoveryCodes": { "type": "array", "items": { "type": "string" } },
            },
            "required": ["provisioningUri", "manualKey", "recoveryCodes"],
        },
        "RecoveryCodes": {
            "type": "object",
            "properties": { "codes": { "type": "array", "items": { "type": "string" } } },
            "required": ["codes"],
        },
    })
}

/// Request bodies.
fn request_schemas() -> Value {
    let role = json!({ "type": "string", "enum": ["owner", "admin", "member"] });

    json!({
        "SignupRequest": {
            "type": "object",
            "properties": {
                "email": { "type": "string", "format": "email" },
                "name": { "type": ["string", "null"] },
            },
            "required": ["email"],
        },
        "LinkRequest": {
            "type": "object",
            "properties": {
                "email": { "type": "string", "format": "email" },
                "purpose": { "type": "string", "enum": ["verify", "recover"] },
            },
            "required": ["email", "purpose"],
        },
        "TokenRequest": {
            "type": "object",
            "properties": { "token": { "type": "string" } },
            "required": ["token"],
        },
        "LoginRequest": {
            "type": "object",
            "properties": {
                "email": { "type": "string", "format": "email" },
                "code": { "type": "string" },
            },
            "required": ["email", "code"],
        },
        "ConfirmTotpRequest": {
            "type": "object",
            "properties": { "code": { "type": "string" } },
            "required": ["code"],
        },
        "CreateOrgRequest": {
            "type": "object",
            "properties": {
                "slug": { "type": "string" },
                "name": { "type": "string" },
            },
            "required": ["slug", "name"],
        },
        "RoleRequest": {
            "type": "object",
            "properties": { "role": role },
            "required": ["role"],
        },
        "InviteRequest": {
            "type": "object",
            "properties": {
                "email": { "type": "string", "format": "email" },
                "role": role,
            },
            "required": ["email"],
        },
        "AcceptInviteRequest": {
            "type": "object",
            "properties": { "token": { "type": "string" } },
            "required": ["token"],
        },
        "CreateTeamRequest": {
            "type": "object",
            "properties": {
                "slug": { "type": "string" },
                "name": { "type": ["string", "null"] },
            },
            "required": ["slug"],
        },
        "TeamPatch": {
            "type": "object",
            "description": "Absent fields are left alone.",
            "properties": {
                "slug": { "type": ["string", "null"] },
                "name": { "type": ["string", "null"] },
            },
        },
        "RegisterRepoRequest": {
            "type": "object",
            "properties": {
                "slug": { "type": "string" },
                "name": { "type": ["string", "null"] },
                "remotes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description":
                        "Any form git prints. Normalized, so SSH and HTTPS forms of \
                         one repository collapse to a single row.",
                },
                "provider": { "type": ["string", "null"] },
                "defaultBranch": { "type": ["string", "null"] },
                "teamId": { "type": ["string", "null"], "format": "uuid" },
                "defaultAgentType": {
                    "type": ["string", "null"],
                    "description": "A free-form hint. Never validated against a list.",
                },
                "trackerBinding": { "type": ["object", "null"] },
            },
            "required": ["slug"],
        },
        "UpdateRepoRequest": {
            "type": "object",
            "description":
                "Absent fields are left alone. The slug is deliberately absent: agents \
                 name it, so renaming would break every skill that does.",
            "properties": {
                "name": { "type": ["string", "null"] },
                "defaultBranch": { "type": ["string", "null"] },
                "teamId": { "type": ["string", "null"], "format": "uuid" },
                "defaultAgentType": { "type": ["string", "null"] },
                "trackerBinding": { "type": ["object", "null"] },
                "active": { "type": ["boolean", "null"] },
                "addRemotes": { "type": "array", "items": { "type": "string" } },
            },
        },
        "MintTokenRequest": {
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "scopes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Defaults to the read-only set when empty.",
                },
                "ttlDays": { "type": ["integer", "null"], "minimum": 1, "maximum": 365 },
            },
            "required": ["name"],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Value {
        document(&catalog())
    }

    /// The drift test. Every schema the catalog names has to exist, or a
    /// generated client gets a dangling `$ref` and fails to build.
    #[test]
    fn every_referenced_schema_is_defined() {
        let doc = doc();
        let schemas = doc["components"]["schemas"].as_object().unwrap();

        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);

        for name in refs {
            assert!(
                schemas.contains_key(&name),
                "the document references #/components/schemas/{name}, which is not defined"
            );
        }
    }

    fn collect_refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key == "$ref" {
                        if let Some(name) = value.as_str().and_then(|r| {
                            r.strip_prefix("#/components/schemas/").map(str::to_string)
                        }) {
                            out.push(name);
                        }
                    }
                    collect_refs(value, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
            _ => {}
        }
    }

    /// An endpoint with no summary is an endpoint nobody outside this repo can
    /// use. Same rule as `df-mcp`'s tool descriptions, for the same reason.
    #[test]
    fn every_endpoint_describes_itself() {
        for endpoint in catalog() {
            assert!(
                !endpoint.summary.is_empty(),
                "{} {} has no summary",
                endpoint.verb.as_str(),
                endpoint.path
            );
        }
    }

    /// An endpoint that claims an org scope but has no `{org}` segment cannot
    /// resolve one — `OrgCtx` would fail at runtime with an internal error.
    /// This is the wiring bug that check exists to report.
    #[test]
    fn org_scoped_endpoints_sit_under_an_org_segment() {
        for endpoint in catalog() {
            if endpoint.auth.needs_org() {
                assert!(
                    endpoint.path_params().contains(&"org"),
                    "{} {} claims {} but has no {{org}} segment",
                    endpoint.verb.as_str(),
                    endpoint.path,
                    endpoint.auth.as_str()
                );
            }
        }
    }

    /// Operation ids become method names in generated clients, so a duplicate
    /// silently overwrites a method.
    #[test]
    fn operation_ids_are_unique() {
        let mut ids: Vec<String> = catalog().iter().map(|e| e.operation_id()).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate operationId in the catalog");
    }

    /// **The rule from the plan, as a test.** No credential is ever spent on a
    /// `GET`: mail scanners and link-preview fetchers follow every URL in every
    /// message, and a single-use `GET` is burned before the human ever clicks
    /// it — a failure that looks exactly like an attack and is not.
    ///
    /// Named endpoints rather than a pattern match, because the property is
    /// about these specific redemptions. Moving one to `GET`, or deleting it,
    /// fails here.
    #[test]
    fn every_single_use_redemption_is_a_post() {
        let redemptions = [
            "/api/auth/verify",
            "/api/auth/recover",
            "/api/orgs/{org}/invites/accept",
            "/oauth/token",
        ];

        let catalog = catalog();
        for path in redemptions {
            let mounted: Vec<_> = catalog.iter().filter(|e| e.path == path).collect();
            assert!(!mounted.is_empty(), "{path} is not mounted at all");

            for endpoint in mounted {
                assert_eq!(
                    endpoint.verb,
                    crate::catalog::Verb::Post,
                    "{path} spends a single-use credential, so it must be a POST — \
                     mail scanners and link previews follow every URL they see"
                );
            }
        }
    }

    /// The other half: the pages those links actually point at are not routes
    /// here at all. `/verify` and `/recover` are console pages that render a
    /// button; the server sees nothing until the button is pressed.
    #[test]
    fn the_emailed_urls_are_pages_not_endpoints() {
        for path in ["/verify", "/recover"] {
            assert!(
                !catalog().iter().any(|e| e.path == path),
                "{path} is the URL that goes in email. It must stay a client-side \
                 page — mounting it as a handler is how a link gets spent by a scanner."
            );
        }
    }

    /// The console watches the queue; it does not drive it. Every write to a
    /// job — enqueue, claim, complete, fail, repend — belongs to the MCP
    /// surface, because the agent doing the work is the only party that can say
    /// when it is done. A "mark complete" button here would let a human tell
    /// the queue something they cannot observe, and the audit trail would
    /// record it as fact.
    #[test]
    fn the_queue_is_read_only_over_the_console() {
        for endpoint in catalog() {
            if endpoint.path.contains("/jobs") {
                assert_eq!(
                    endpoint.verb,
                    crate::catalog::Verb::Get,
                    "{} {} writes to the queue from the console",
                    endpoint.verb.as_str(),
                    endpoint.path
                );
            }
        }
    }

    #[test]
    fn the_document_is_openapi_31_with_a_session_scheme() {
        let doc = doc();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(
            doc["components"]["securitySchemes"]["sessionCookie"]["name"],
            crate::session::COOKIE_NAME
        );
        assert!(doc["paths"]["/api/me"]["get"]["security"].is_array());
        assert!(
            doc["paths"]["/api/auth/login"]["post"]
                .get("security")
                .is_none(),
            "a public endpoint must not require a session"
        );
    }

    /// One path serving several methods must render as one entry with several
    /// operations, not as the last one written.
    #[test]
    fn a_path_with_several_methods_keeps_all_of_them() {
        let doc = doc();
        let sessions = &doc["paths"]["/api/me/sessions"];
        assert!(sessions["get"].is_object(), "GET was lost");
        assert!(sessions["delete"].is_object(), "DELETE was lost");
    }
}
