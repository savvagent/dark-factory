//! Organization tools.
//!
//! `whoami` is the tool an agent should call first in a session, and it is the
//! answer to a question no other tool answers: *which* of a person's
//! organizations does this token open? A user may belong to several; a token
//! opens exactly one, fixed when it was issued and not selectable per call. An
//! agent that assumes otherwise queues work in the wrong company's queue.
//!
//! The usage and quota fields land here with metering (milestone 1 task 9).

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::server::{Factory, McpResult};

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

#[tool_router(router = org_router, vis = "pub(crate)")]
impl Factory {
    #[tool(
        name = "whoami",
        description = "Who this token says you are: your user, the one organization it opens, \
                       your role in it, and the scopes it carries. Call this first in a \
                       session — it is the only way to know which organization your work will \
                       land in, and which tools you are allowed to use."
    )]
    pub async fn whoami(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::WhoAmI>, ErrorData> {
        let caller = self.caller(&parts)?;

        // No scope check. A token that cannot ask what it is cannot report a
        // useful error either, and there is nothing here the holder of the
        // token does not already have.
        let user = self.db().get_user(caller.user_id).await.mcp()?;
        let org = self.db().get_org(caller.org_id).await.mcp()?;
        let role = self
            .db()
            .member_role(caller.org_id, caller.user_id)
            .await
            .mcp()?;

        Ok(Json(out::WhoAmI {
            user: out::UserOut {
                id: caller.user_id,
                email: user.as_ref().map(|u| u.email.clone()),
                name: user.as_ref().and_then(|u| u.name.clone()),
            },
            org: out::OrgOut {
                id: caller.org_id,
                slug: org.as_ref().map(|o| o.slug.clone()),
                name: org.as_ref().map(|o| o.name.clone()),
                plan: org.as_ref().map(|o| o.plan),
            },
            role,
            token: out::TokenOut {
                kind: match caller.kind {
                    df_auth::tokens::TokenKind::Oauth => "oauth",
                    df_auth::tokens::TokenKind::Pat => "pat",
                },
                client_id: caller.client_id,
                scopes: caller.scopes,
                expires_at: caller.expires_at,
            },
        }))
    }
}
