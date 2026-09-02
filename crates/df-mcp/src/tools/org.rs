//! Organization tools.
//!
//! `whoami` is the tool an agent should call first in a session, and it is the
//! answer to a question no other tool answers: *which* of a person's
//! organizations does this token open? A user may belong to several; a token
//! opens exactly one, fixed when it was issued and not selectable per call. An
//! agent that assumes otherwise queues work in the wrong company's queue.
//!
//! Both tools here are free, and deliberately so: a caller must never have to
//! spend an operation to find out how many it has left.

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
                       your role in it, the scopes it carries, and how much of this month's \
                       allowance is left. Call this first in a session — it is the only way to \
                       know which organization your work will land in, and which tools you are \
                       allowed to use."
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

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "whoami").await?;
        let usage = self.meter().report(&mut tx).await.mcp()?;
        tx.commit().await.mcp()?;

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
            usage,
        }))
    }

    #[tool(
        name = "usage",
        description = "How much of this organization's monthly allowance has been used, and how \
                       much is left. You are billed for work performed, not for looking: \
                       reads, watch and lease renewals are free, while queueing, claiming, \
                       completing and messaging consume the allowance. This tool is free. If \
                       `warning` is true, tell the person you are working for — someone with \
                       billing access may need to act before work starts being refused."
    )]
    pub async fn usage(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::UsageOut>, ErrorData> {
        let caller = self.caller(&parts)?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "usage").await?;
        let usage = self.meter().report(&mut tx).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::UsageOut { usage }))
    }
}
