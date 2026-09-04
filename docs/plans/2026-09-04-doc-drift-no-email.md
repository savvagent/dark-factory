# Doc drift from removing email — cleanup

**Fast-path: no design spec per dark-factory-development trivial-task criteria** — this is a
documentation-only correction with no code or interface change, closing GitHub issue #14.

## Goal

`docs/plans/2026-09-01-milestone-1.md` (Tasks 6, 10, 11, 13),
`docs/specs/2026-09-01-dark-factory-design.md` (§ Layer 2 and the crate table/deployment
sections), and `docs/deploy/cloudflare.md` still describe the TOTP + emailed magic-link
authentication mechanism that `Remove email from the product` (`0009_no_email`) and
`Replace TOTP with passkeys` (`0010_passkeys`) replaced. The task brief also named a stale
comment in `crates/df-auth/src/totp.rs:12` — that file no longer exists (the module was
deleted along with TOTP), so no source change is needed there.

## Task 1 — Correct the drifted docs

- [x] Annotate `docs/plans/2026-09-01-milestone-1.md` Tasks 6, 10, 11, and 13 so each
      passage describing TOTP enrollment, magic-link delivery, `LogMailer`, or the
      emailed-verification signup flow is marked as superseded, with a pointer to the
      commits (`Remove email from the product`, `Replace TOTP with passkeys`) and to
      `CLAUDE.md`'s Authentication section for the mechanism as it stands today. Historical
      narrative (what actually shipped and was verified at the time) is kept, not deleted.
- [x] Update `docs/specs/2026-09-01-dark-factory-design.md`'s "Layer 2 — proving who the
      human is" section, the architecture diagram, the crate responsibility table, the
      deployment secrets line, and the testing bullet to describe passkeys instead of TOTP
      + email.
- [x] Update `docs/deploy/cloudflare.md`'s references to emailed links and magic links to
      reflect the current passkey ceremony, annotating the "Verified locally" section as
      predating the removal.
- [x] Confirm no stale TOTP/magic-link/`LogMailer` comments remain in `crates/df-auth/src/`.
- [x] `git commit -m "docs: fix doc drift from removing email (#14)"`.

No test/lint/build gate applies — documentation only, no code changed.
