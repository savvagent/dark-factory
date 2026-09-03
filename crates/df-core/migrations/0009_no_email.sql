-- Remove email from the product.
--
-- dark-factory sends no mail. An authenticator app is the only factor, recovery
-- codes are the only self-service way back in, and an org admin resetting a
-- member's credential is the only assisted one. Invitations travel as codes the
-- admin hands over themselves, and enterprises federate through OIDC.
--
-- Two things go with that decision, and both are dropped rather than left
-- present-and-always-null. A column nobody writes is worse than an absent one:
-- it reads as a feature that exists, and the next person to touch this schema
-- has to work out whether the emptiness is a bug.
--
-- What this deliberately does **not** drop is `users.email`. The address stays
-- as the account's unique key — it is what someone types to sign in, what an
-- admin types to invite a colleague, and what makes an audit row legible. It is
-- simply never sent to, and never proved.

-- Single-use links for verification, recovery, and invitation acceptance.
-- Nothing issues or consumes these any more: `df_auth::magic` is gone, and the
-- invitation token lives in `org_invites.token_hash`, which is unaffected.
DROP TABLE IF EXISTS magic_links;
DROP TYPE IF EXISTS magic_link_purpose;

-- Nothing can set this any more, because proving control of an address requires
-- sending something to it.
--
-- The one place it was load-bearing was the gate on creating an org: an
-- unverified address must not be able to claim a public slug. That check now
-- asks for a confirmed authenticator instead, which is a *stronger* statement
-- about the account than a clicked link ever was — see `df_web::routes::orgs`.
ALTER TABLE users DROP COLUMN IF EXISTS email_verified_at;
