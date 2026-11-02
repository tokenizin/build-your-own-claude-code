# GDPR Compliance Specification — Login Feature

> **Reference:** DISCUSSION-001 (GDPR compliance review)  
> **Primary Domain:** [auth](/docs/domains/auth/OVERVIEW.md)  
> **Related Spec:** [docs/features/auth/SPECIFICATION.md](../auth/SPECIFICATION.md)  
> **Author Business Analyst | Date Created:** 2025-10-31  
> **Status:** Proposed → Ready for Review  

---

## 1. Purpose & Scope

This specification translates the GDPR compliance requirements from **DISCUSSION-001** into detailed functional and non-functional requirements, user stories, and acceptance criteria governing the login/authentication feature. Its goal is to ensure every data handling, consent, encryption, and erasure operation within the auth flow satisfies EU General Data Protection Regulation (GDPR) obligations — specifically the principles of **data minimization**, **encryption at rest/in transit**, **granular explicit consent**, **auditability**, and the **Right to Be Forgotten** (Article 17).

This spec applies to:
- Login / registration UX & APIs
- Server-side data storage, encryption, and key management
- Consent capture, granular toggle flows, and audit logging
- Right-to-erasure / anonymization workflows

---

## 2. Applicable GDPR Articles

| Article | Obligation | Covered in This Spec |
|---------|-------------|-----------------------|
| Art. 5(1)(c) | Data minimization | §3 & User Stories 1–2 |
| Art. 5(1)(f) | Integrity & confidentiality (encryption) | §4, §6 & User Stories 3–4 |
| Art. 7 | Conditions for consent | §5, §6 & User Stories 5–7 |
| Art. 12–14 | Transparent information & right to be informed | User Story 8 |
| Art. 16 | Right to rectification | User Story 9 |
| Art. 17 | Right to erasure ("right to be forgotten") | §6 & User Stories 10–13 |
| Art. 25(1) | Data protection by design & default | Cross-cutting in all user stories |
| Art. 30 | Records of processing activities (audit trail) | User Story 7 |

---

## 3. Data Minimization Strategy — Functional Requirements

### FR-DM-01: Minimal Login Payload

The login endpoint MUST accept only `(email, password)` or `(email_hash, salt_response)` as input fields during core authentication. No PII (name, age, demographics, phone number) may be requested at the login/registration step.

### FR-DM-02: Decoupled Profile-update Flow

PII collection MUST occur in a separate `/profile/update` flow that is only accessible after successful authentication and requires an **explicit secondary consent prompt**. Non-submission of optional profile attributes SHALL NOT block access to core platform functionality.

### FR-DM-03: Default Refusal Enforcement

If the user declines optional profile fields, the system MUST persist only the essential account record (email identifier + auth hash) without error or degraded auth performance.

---

## 4. Encryption — Functional & Non-Functional Requirements

### FR-ENC-01: Encryption at Rest

All sensitive columns in the database (passwords/hashes, salts, email hashes, session tokens) MUST be stored using **AES-256-GCM** for field-level encryption plus **Bcrypt or Argon2 with cost ≥ 12** for credential hashing. Plain-text passwords are prohibited.

### FR-ENC-02: Encryption in Transit

Every HTTP request involving authentication data MUST use TLS 1.3+ (HSTS enabled). Self-signed or TLS < 1.2 certificates SHALL NOT be used in production.

### FR-ENC-03: Key Management & Rotation

Encryption keys SHALL be managed outside the application server via an approved KMS (AWS KMS / HashiCorp Vault / equivalent) with:
- Automatic key rotation **at least annually** (maximum 365-day lifespan).
- Versioned key IDs stored alongside encrypted fields to permit retrograde decryption when needed.
- No raw private keys committed to source control or environment variables as plain text.

---

## 5. Consent Flows — Functional Requirements

### FR-CNS-01: Active Opt-In Default

All consents ("Marketing," "Analytics," "Third-Party Sharing") MUST default to **unchecked / off**. Pre-ticked boxes are non-compliant and SHALL NOT appear in any UI.

### FR-CNS-02: Granular Consent Toggles

Users MUST be presented with three separate toggles:
| Toggle | Description |
|--------|-------------|
| Essential | Required for platform functionality; cannot be disabled |
| Performance | Aggregates analytics, usage telemetry, performance monitoring |
| Marketing | Personalized advertising, promotional emails, third-party sharing |

These cannot be grouped under a single "Accept All" gate. The "Essential" toggle may display as permanently on but must be visually distinguishable.

### FR-CNS-03: Consent UI Placement

The consent prompt MUST appear **before** the user is granted post-login access to platform features (i.e., within the initial session or a modal overlay). A dismissible CTA ("Review & Manage Consents Later") provides at least one screen of delay for users who wish to configure preferences before proceeding.

### FR-CNS-04: Auditability of Consent State Changes

Every consent state mutation MUST produce an immutable log record containing:
| Field | Type | Description |
|-------|------|-------------|
| `user_id` | UUID | Authenticated user ID |
| `consent_type` | enum | `[essential, performance, marketing]` |
| `old_value` | boolean | Previous state (true/false) |
| `new_value` | boolean | Current state (true/false) |
| `ip_address` | string | Client IP at time of change |
| `user_agent` | string | Browser/device identifier |
| `timestamp` | ISO-8601 UTC | Server-side timestamp |

This audit log SHALL be append-only and retained for the life of the user account plus 24 months after erasure.

---

## 6. Right to Erasure / Anonymization — Functional Requirements

### FR-ERAS-01: Destructive API Endpoint

A **`POST /auth/forgive`** endpoint (or equivalent name) MUST exist for initiated irreversible deletion.

Request schema:
```json
{
  "confirmation": "STRONG_PASSWORD_VERIFICATION",
  "type": "deletion"
}
```

Response schema:
```json
{
  "status": "initiated | denied",
  "message": "...",
  "expiry_days": 30
}
```

The user MUST re-authenticate (provide their password a second time) to confirm intent, preventing accidental or unauthorized erasure.

### FR-ERAS-02: Cascade Erasure Logic

Upon confirmed erasure request:
1. Delete the user record from the active database **within 24 hours**.
2. Trigger cryptographic wiping with random padding / overwriting (crypto-shred).
3. Instruct backup systems to purge references within **30 days** of the request timestamp.
4. If any linked records require retention for legal purposes (e.g., financial transactions, fraud investigation), the user MUST be explicitly informed pre-erasure which data will remain and why.

### FR-ERAS-03: Anonymization Fallback

If immediate full erasure is not operationally feasible (active transaction / legal hold):
- The system MUST offer an **"Anonymize" button** in the user settings UI.
- Anonymization strips PII (email, name, IP) by replacing with irreversible `null` or randomized pseudonyms while retaining transactional records under **Article 6(1)(c)** legal obligation exception.
- The anonymized record must be **irreversible** — no technique shall allow re-identification of the original user from the anonymized data.
- Anonymization event is logged identically to erasure audit entries (see § FR-CNS-04).

### FR-ERAS-04: User-Facing Flow for Deletion

The deletion / anonymization flow in the UI MUST include:
1. A confirmation banner with bold language: *"This action is permanent and cannot be undone."*
2. A text input field where the user must type **"DELETE MY ACCOUNT"** to enable the destructive button.
3. An email sent to the account's registered contact confirming erasure initiation and 7-day grace period for revocation (email link `POST /auth/forgive/revise`).
4. A final confirmation email delivered **after** full erasure completes.

---

## 7. Right to Data Export (Article 20 — Portability)

### FR-EXPORT-01: User Data Export Endpoint

A **`GET /auth/export`** endpoint MUST support automated download of all user-held data in a structured, machine-readable format (JSON/CSV).

Response includes:
| Category | Content |
|----------|---------|
| Account info | Email, account creation date, active consent states |
| Profile data | All fields submitted through `/profile/update` |
| Consent history | Full audit trail from FR-CNS-04 |
| Transaction log | Purchase / activity records (redacted IDs where third-party) |

Download MUST expire after 15 minutes and be limited to **two requests per rolling 24-hour period** to prevent abuse.

### FR-EXPORT-02: Export UI Placement

The export link must appear under **Settings → Privacy & Data** alongside the erasure options with descriptive copy: *"Download a copy of all your data"* and *"Request deletion of my account"*.

---

## 8. Right to Rectification (Article 16)

### FR-RECT-01: Editable Profile Fields

Authenticated users MUST be able to update their email address via a **`PATCH /profile/email`** endpoint that:
- Requires password re-authentication
- Sends a confirmation link to the old and new email addresses
- Retains the previous email in an `email_history` table (append-only, max 5 entries) for identity verification

### FR-RECT-02: Immutable Consent Audit Trail

Consent records SHALL NOT be editable or deletable by users; rectification of consent applies only to **new submissions** (creating a new appended record with the updated state).

---

## 9. User Stories & Acceptance Criteria

### User Story US-GDPR-01 — Minimal Login Data Capture

> As a **user**, I want to log in using only my email and password so that I am not forced to provide unnecessary personal information.

| AC ID | Description |
|-------|-------------|
| **AC-1.1** | The login form exposes exactly two fields: `email` and `password`. No PII fields are visible or submitted during authentication. |
| **AC-1.2** | Registering a new account requires only `(email, password)`; no name, age, or demographic inputs may be enforced as required fields. |
| **AC-1.3** | The API route `POST /auth/login` rejects or ignores any PII body parameters not defined in the minimal schema (e.g., extra keys `name`, `age`). |
| **AC-1.4** | Core platform functionality is fully accessible immediately upon successful login without profile completion. |

### User Story US-GDPR-02 — Decoupled Optional Profile Collection

> As a **user**, I want to complete my profile later so that I can use the app immediately with minimal friction.

| AC ID | Description |
|-------|-------------|
| **AC-2.1** | After login, the user sees an optional `/profile/update` prompt with a dismissible CTA ("Skip for now — update later in Settings"). |
| **AC-2.2** | All profile fields are marked `optional` in the schema / validator; no field triggers a form submission error if left blank. |
| **AC-2.3** | A secondary explicit consent check appears when the user first submits any PII to the profile-update flow. |

### User Story US-GDPR-03 — Encryption at Rest Verification

> As an **administrator**, I want all sensitive auth data encrypted in the database so that a compromised DB never exposes plain-text credentials or private identifiers.

| AC ID | Description |
|-------|-------------|
| **AC-3.1** | Database query of `users.password_hash` returns a Bcrypt/Argon2 hash (not raw text); querying any PII column returns encrypted blobs identifiable by a non-decryptable string prefix. |
| **AC-3.2** | Application logs NEVER print full passwords, salts, or decryption keys at any severity level (`debug` through `error`). |
| **AC-3.3** | Database dump (`.sql` / `.json` export) contains only encrypted values — verifiable by manual inspection of sample rows. |

### User Story US-GDPR-04 — Encryption in Transit Enforcement

> As an **administrator**, I want all authentication traffic over TLS so that credentials cannot be intercepted on the network.

| AC ID | Description |
|-------|-------------|
| **AC-4.1** | Any HTTP request to `*/auth/*` on port 80 (HTTP) returns a `301 → https://...` redirect; no plain-text POST body is accepted over HTTP. |
| **AC-4.2** | TLS certificate in production is valid and uses TLS 1.3; `sslscan` or equivalent tool reports cipher suites ≥ TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384. |
| **AC-4.3** | `Strict-Transport-Security` header is present on every response with `max-age ≥ 31536000; includeSubDomains`. |

### User Story US-GDPR-05 — Granular Consent Toggles

> As a **user**, I want separate toggles for different data collection types so that I can control exactly what information is shared.

| AC ID | Description |
|-------|-------------|
| **AC-5.1** | The consent UI renders three mutually independent toggle switches: Essential, Performance, Marketing — all defaulting to `off` (except Essential which displays as permanently on but remains visually distinct). |
| **AC-5.2** | No "Accept All" or "Reject All" group button is present; toggles are individually actionable. |
| **AC-5.3** | Selective save works: e.g., user can toggle Marketing `on` while leaving Performance `off`. The persisted state reflects this exactly. |
| **AC-5.4** | If the user dismisses consent without selecting, their stored state defaults to `[essential=false, performance=false, marketing=false]` and a modal is re-displayed within 24 hours of next session. |

### User Story US-GDPR-06 — Active Opt-In Enforcement

> As an **administrator**, I want no pre-consented data flows so that the application complies with GDPR's active consent requirement.

| AC ID | Description |
|-------|-------------|
| **AC-6.1** | Front-end source code is inspected: zero `<input type="checkbox" checked>` elements related to consent or data-sharing categories exist. |
| **AC-6.2** | The backend seed / migration for first-time users sets all opt-in consents to `false` in the database on initial account creation. |
| **AC-6.3** | API endpoint `POST /auth/consent/prefill` does NOT exist; no code path auto-enables any opt-in toggle without user action. |

### User Story US-GDPR-07 — Consent Audit Trail

> As a **compliance officer**, I want an immutable record of every consent change so that we can prove compliance during an audit.

| AC ID | Description |
|-------|-------------|
| **AC-7.1** | Every `PUT /auth/consent` call appends (not updates) a new row to the `consent_audit_log` table with all fields defined in FR-CNS-04. |
| **AC-7.2** | The audit log uses an append-only DB constraint or immutable storage pattern; rows cannot be edited, truncated, or deleted by application code. |
| **AC-7.3** | Audit logs persist for the life of the account plus 24 months post-deletion and are available for export via `GET /auth/export`. |

### User Story US-GDPR-08 — Right to Be Forgotten (Destructive Deletion)

> As a **user**, I want to permanently delete my account so that my personal data is irreversibly removed from the system.

| AC ID | Description |
|-------|-------------|
| **AC-8.1** | Destructive deletion is initiated via `POST /auth/forgive` with password re-authentication; the endpoint returns `{ "status": "initiated" }` upon success. |
| **AC-8.2** | Upon confirmed initiation, all active session tokens for that user are immediately revoked (`/auth/revoke-all`). |
| **AC-8.3** | The user's primary DB record is deleted from the `users` collection within 24 hours of initiation (verified by a cron / job scheduler). |
| **AC-8.4** | Cryptographic shredding of encrypted fields uses random padding overwrite; verified by asserting the post-shred column value reads as all-zero or random bytes on recovery. |

### User Story US-GDPR-09 — Cascade Erasure Notification

> As a **user requesting deletion**, I want to be clearly informed about what is being permanently deleted and receive confirmation when erasure completes so that the process is transparent.

| AC ID | Description |
|-------|-------------|
| **AC-9.1** | Before deletion, a confirmation banner displays: *"This action is permanent and cannot be undone."* with a text prompt requiring the user to type `DELETE MY ACCOUNT`. |
| **AC-9.2** | A revocation mechanism exists: upon initiation, a 7-day grace period email (from `[privacy@domain]`) includes a link `POST /auth/forgive/revise` to cancel before erasure completes. |
| **AC-9.3** | After erasure succeeds, a second confirmation email is sent with subject line: *"Account Deletion Confirmed — [Account Name]"*.  |
| **AC-9.4** | If legal hold records exist (financial/fraud logs), the user receives a third email explicitly stating which data will be retained and the legal basis for retention prior to erasure initiation. |

### User Story US-GDPR-10 — Anonymization Fallback

> As a **user**, I want an option to anonymize my account if full deletion would break related records so that some of my data is preserved while my identity is irreversibly removed.

| AC ID | Description |
|-------|-------------|
| **AC-10.1** | The Settings → Privacy page displays both *"Delete My Account"* and *"Anonymize My Account"*.  |
| **AC-10.2** | Upon selecting anonymization, email/name/IP columns are irreversibly replaced with `null` or randomized pseudonyms (non-recoverable). |
| **AC-10.3** | Transactional/financial records remain but under a new non-identifying `user_id` alias; no foreign-key cross-reference to the original PII exists post-anonymization. |
| **AC-10.4** | The anonymization event is logged identically to the consent audit trail (FR-CNS-04 format) with `consent_type: "anonymization"`. |

### User Story US-GDPR-11 — Data Portability Export

> As a **user**, I want to download a copy of all my personal data in a machine-readable format so that I can move it to another service (GDPR Article 20).

| AC ID | Description |
|-------|-------------|
| **AC-11.1** | Endpoint `GET /auth/export` generates a JSON response containing account info, profile data, consent history, and transaction log categories as defined in FR-EXPORT-01. |
| **AC-11.2** | The exported file is downloadable within 15 minutes of request generation; the download link expires afterward. |
| **AC-11.3** | The endpoint enforces a maximum of two requests per rolling 24-hour period (tracked via `user_id + timestamp`); additional requests return HTTP 429 with `"Rate limited: 1 attempt per 12 hours"`. |
| **AC-11.4** | No PII of other users (e.g., recipient names in transactions) appears in the export; fields are redacted using `[REDACTED]` markers. |

### User Story US-GDPR-12 — Key Rotation Verification

> As an **administrator**, I want encryption keys to rotate automatically at least annually so that compromised keys cannot remain valid indefinitely (GDPR Art. 32).

| AC ID | Description |
|-------|-------------|
| **AC-12.1** | KMS configuration enforces `rotation_period_days = 365` (maximum). The system SHALL NOT accept periods > 365 days. |
| **AC-12.2** | On a new key version, the `encryption_key_id` field for affected rows is updated atomically via a background migration job that re-encrypts all records under the rotated key. |
| **AC-12.3** | No raw KMS secrets exist in environment variables or source code; access uses IAM roles / Vault tokens exclusively. |

### User Story US-GDPR-13 — Right to Rectification

> As a **user**, I want to update my email address accurately so that my account stays secure and reachable (GDPR Article 16).

| AC ID | Description |
|-------|-------------|
| **AC-13.1** | The `PATCH /profile/email` endpoint requires the current password in the request body for re-authentication. |
| **AC-13.2** | Post-update, confirmation emails are sent to both the old and new addresses; the email update only applies after clicking links in both emails. |
| **AC-13.3** | The `email_history` table stores up to 5 prior emails (append-only); the system enforces a maximum of 5 without deleting any older entry. |

---

## 10. Non-Functional Requirements Checklist

| ID | Requirement | Metric / Verification |
|----|-------------|-----------------------|
| **NFR-01** | Erasure completion time ≤ 24 hours (primary DB); ≤ 30 days (backups) | Automated audit log review |
| **NFR-02** | Consent toggle latency ≤ 500ms user-perceived | Load / responsiveness test |
| **NFR-03** | 100% of consent mutations auditable | Log analysis: no gaps in `consent_audit_log` |
| **NFR-04** | TLS 1.3 enforced on all auth endpoints | `sslyze` or equivalent scan returns pass |
| **NFR-05** | Encryption at rest coverage = 100% of sensitive columns | Static code analysis + DB schema inspection |
| **NFR-06** | Export file format is valid JSON adhering to draft schema | Schema validation script pass rate ≥ 100% |

---

## 11. Data Flow Diagram (Textual)

```
┌──────────────┐    login POST /auth/login
│   User/Client ──►│          │
└───────┬──────┘    │          │
        │  TLS 1.3  ▼          │
        │  ┌───────────────────────┐
        │  │ Auth Gateway (HSTS)   │
        │  │ - Validate schema     │
        │  │ - Reject PII extras   │
        │  └───────┬───────────────┘
        │          ▼
        │  ┌───────────────────────┐
        │  │ Auth Service           │
        │  │ - bcrypt verify        │
        │  │ - AES-256-GCM decrypt  │
        │  └───────┬───────────────┘
        │          ▼
        │  ┌───────────────────────┐
        ████████████████████████████│  ← Encrypted DB (AES-256-GCM)
         ██████████████████████████
        │                          │
        │                  POST /auth/forgive (erasure)
        │                           │
        ▼                    ┌─────▼────────┐
   ┌──────────────┐      │  Erasure Job    │
   │ Backup System│◄─────│  - crypto shred  │
   │  (purge 30d) │      │  - cascade del  │
   └──────────────┘      └────────────────┘
```

---

## 12. Open Questions / Assumptions

| # | Question | Decision Pending |
|---|----------|------------------|
| OQ-01 | Should the `POST /auth/forgive` endpoint also support "soft-delete" with a configurable retention window (e.g., 7, 30, 90 days)? | PMA: Yes — default to immutable deletion. Soft-delete must be an additive option post-CRUD-PLAN. BA will add this as an appendix if the team confirms engineering cost ≤ 1 sprint. |
| OQ-02 | Does the system handle EEA users only, or is GDPR applied globally? | Assumed global policy (best practice). If regional gating is needed later, a separate `data_residency` feature SCR is required. |
| OQ-03 | Which KMS platform will be used (AWS KMS vs HashiCorp Vault)? | Tech Lead to confirm during architecture implementation SCR. Both are acceptable; spec remains vendor-neutral for now. |

---

## 13. Acceptance Criteria Traceability Summary

All ACs (AC-1.1 through AC-13.4) are **testable** and must pass in automated E2E/CI test suites or manual verification scripts before this feature can be marked `Implemented` in the SCR lifecycle. Key verification commands:

| Criterion Group | Verification Method |
|-----------------|--------------------|
| AC-1.x (minimization) | Frontend form inspection + API payload schema test |
| AC-3.x / AC-4.x (encryption) | DB row inspection + TLS scan (`sslyze`) + log output audit |
| AC-5/6.x (consent) | UI E2E test asserting unchecked defaults + backend seed validation |
| AC-7.x (audit trail) | Append-only constraint verification + log integrity check |
| AC-8–10.x (erasure/anonymize) | Destructive API test + post-op DB row count = 0 |
| AC-11.x (export) | `GET /auth/export` payload validates against schema; rate-limit test at requests 3+ |
| AC-12.x (key rotation) | KMS config inspection + automated migration test |
| AC-13.x (rectification) | Email confirmation E2E flow + email_history bounded to 5 rows |

---

## 14. Documentation Impact

This spec creates the **GDPR-specific** layer for the auth feature. The following documents require update upon implementation completion:

| Document | Change Required | Owner |
|----------|-----------------|-------|
| `docs/features/auth/SPECIFICATION.md` | Add a new "🔒 GDPR Compliance" section referencing this file as authority; link to each FR section | BA |
| `docs/domains/auth/RULES.md` | Add data-minimization & consent rules as invariant principles | BA |
| `docs/architecture/TECHNICAL_ARCHITECTURE.md` | Append encryption strategy, KMS integration diagram, and delete/anonymize flow to the persistence section | Architect |
| `docs/product/FEATURES_LIST.md` | Add "GDPR Compliance" under the auth feature as a top-level line item | BA |

---

## 15. References

- [DISCUSSION-001](/tasks/discussions/DISCUSSION-001-gdpr-compliance-review.md) — GDPR compliance review discussion transcript
- GDPR Regulation (EU) 2016/679
- OWASP Authentication Cheat Sheet
- NIST SP 800-111 (Guide to Encrypting AWS Data)

---

*End of Specification*
