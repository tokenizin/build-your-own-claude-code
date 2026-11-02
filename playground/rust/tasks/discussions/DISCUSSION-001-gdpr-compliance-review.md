---
id: DISCUSSION-001
title: GDPR compliance review
status: active
agent: playground
session_id: playground-19949
---

# Discussion: GDPR compliance review

## Messages

**User**
Starting this session to discuss GDPR compliance requirements specifically for our login feature. We need to cover data retention policies, explicit consent mechanisms, the right to be forgotten/archival, server-side vs client-side storage of auth tokens/cookies, and how user-facing flows need to reflect these privacy controls.

**User**
**Discussion Follow-up: GDPR Compliance Requirements for Login Feature**

As Document Steward, the following compliance requirements must be integrated into the implementation specs. Please ensure the developers and architects address these specific points in `docs/features/auth/SPECIFICATION.md`.

### 1. Data Minimization Strategy
*   **Strictest Scope:** The login flow MUST capture only data strictly necessary for authentication (e.g., email hash, password salt) at entry. PII (name, age, demographic data) must be decoupled into an optional `profile-update` flow that requires a secondary consent step.
*   **Default Refusal:** Users must not be blocked from core functionality based on their refusal to provide non-essential profile attributes.

### 2. Encryption at Rest & in Transit
*   **Database Storage:** User credentials and identifiers stored in the database (PostgreSQL/MongoDB) MUST utilize AES-256-GCM encryption for sensitive columns. Plain-text passwords are strictly prohibited; use Bcrypt or Argon2 with a minimum cost factor of 12.
*   **Key Management:** Encryption keys must be distinct from the data and managed via a KMS (e.g., AWS KMS or HashiCorp Vault) with automatic rotation capabilities at least annually.

### 3. Consent Flows & Granularity
*   **Active Opt-In:** All marketing, analytical, and third-party sharing consents MUST be opt-in by default. Pre-ticked checkboxes are non-compliant.
*   **Auditability:** Every consent state change must be logged immutably (creating/updated timestamp) to provide a verifiable audit trail during compliance reviews.
*   **Granularity:** Users must have separate toggles for "Essential," "Performance," and "Marketing" cookies/data collection; these cannot be lumped into a single "Accept All."

### 4. Right-to-Be-Forgotten (Article 17) Implementation
*   **Destructive API Endpoint:** Implement a `POST /auth/forgive` (or similar destructive endpoint) that initiates an irreversible deletion process.
*   **Cascade Erasure:** On approval, the user’s record must be cryptographically wiped or permanently deleted from the active DB and all backups within 30 days of request.
*   **Anonymization Option:** If immediate deletion is not operationally feasible during a session (e.g., active financial transaction), provide an "Anonymize" option that strips PII while retaining transactional history for legal record-keeping mandates, ensuring the user can no longer be re-identified.

**Action Required by Architect/Dev team:** Please add a section to the Tech Specs detailing the data architecture and deletion workflows above.
