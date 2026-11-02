# GDPR Implementation Plan — Login Feature

## 1. /auth/forgive Endpoint
- Route: POST /auth/forgive
- Request: { confirmation: STRONG_PASSWORD_VERIFICATION, type: deletion }
- Flow: password re-auth → 7-day grace → cascade deletion → confirmation email

### Implementation Details
- **Password Re-authentication**: Validate current password via backend crypt check before initiating any irreversible action. Rejects requests where credentials are invalid or expired.
- **7-Day Grace Period**: Upon initiation, the account status transitions to `PENDING_DELETION`. The user retains full access for 168 hours. A countdown email is sent at day 3 and day 6.
- **Cascade Deletion**: After 7 days, a backend cron job (or event-driven worker) purges personal data from all tables (users, profiles, sessions, audit logs). Anonymized transaction records are retained per financial compliance laws.
- **Confirmation Email**: Upon deletion completion, sends `noreply@starworld.io` with subject "Your Account Has Been Removed" including an unsubscribe link for future marketing touchpoints only.

## 2. Encryption at Rest
- Password hashing: Bcrypt/Argon2 cost ≥ 12
- Sensitive columns: AES-256-GCM
- Key management: KMS with annual rotation

### Implementation Details
- **Password Hashing**: Replace legacy hashing (if any) with Argon2id or bcrypt with a minimum work factor of 12. Cost parameters must be configurable and auditable to meet future security standards without breaking existing hashes.
- **AES-256-GCM Sensitive Columns**: Encrypt PII fields (`email`, `phone`, `ip_address`) at the application layer before database write. Database records store only ciphertext. Decryption occurs only in memory during read operations and is wiped immediately after use.
- **KMS Key Management**: Utilize AWS KMS (or equivalent) for envelope encryption. Master keys are rotated annually via automated key rotation policies. A backup key ring is maintained in a separate region with no auto-destruction permissions.

## 3. Consent Toggles
- UI: Essential/Performance/Marketing toggles
- Backend: PUT /auth/consent with audit log
- Default: all opt-in false

### Implementation Details
- **UI Toggles**: Implement three distinct stateful switches in the account settings panel:
  1. **Essential**: Pre-checked and non-deletable (functional requirements for service delivery).
  2. **Performance**: Allows anonymized analytics to be collected for feature improvements.
  3. **Marketing**: Enables personalized ads, newsletters, and cross-network promotions.
- **Backend Consent Endpoint**: `PUT /auth/consent` requires the current password to prevent unauthorized revocation/toggling. Upon success, records an immutable audit entry with versioned consent states.
- **Default State**: All accounts created post-implementation deploy with `Essential=true`, `Performance=false`, `Marketing=false`. Existing data migrates via a prompt-on-first-login flow to avoid mass false-consent events.

## 4. Data Minimization
- Remove PII from login schema
- Add optional profile-update flow
- Secondary consent prompt

### Implementation Details
- **Login Schema Cleanup**: Strip `email` and `phone_number` columns from the primary `users` table used strictly for session management. Use a dedicated `auth_credentials` table or UUID-based anonymous mapping to limit exposure if the login layer is compromised.
- **Optional Profile Update Flow**: Decouple PII collection from registration. New users are assigned an anonymous identifier initially. A profile update modal (triggered after first successful authentication) asks for name, email, and avatar with explicit consent checkboxes. This ensures we only store what the user actively provides.
- **Secondary Consent Prompt**: On first login post-deployment, trigger a non-skippable modal overlay detailing exactly what data was previously stored and requesting fresh reaffirmation of `Performance` and `Marketing` preferences before standard UI access is granted for that session.

## 5. Phased Rollout
- Phase 1: Foundation (DB schema, KMS setup)
- Phase 2: Core (encryption, consent endpoints)
- Phase 3: UI (toggles, deletion flow)
- Phase 4: QA (tests, audit)

### Milestones and Deliverables
**Phase 1: Foundation (Weeks 1-3)**
- Provision KMS keys and configure rotation policies.
- Draft and apply database migration scripts for partitioning PII into segregated tables/columns.
- Set up automated backup and disaster recovery protocols for the new encrypted schema.

**Phase 2: Core Development (Weeks 4-7)**
- Implement Argon2/bcrypt hashing and AES-256-GCM application-layer encryption logic.
- Build `POST /auth/forgive` and `PUT /auth/consent` API endpoints.
- Write backend unit tests for cryptographic integrity and cascade workflows.

**Phase 3: UI Implementation (Weeks 8-10)**
- Develop the consent toggle panel in the settings drawer.
- Build the `PENDING_DELETION` countdown modal and the initial "Reaffirm Consent" overlay.
- Ensure all UI changes meet WCAG AA accessibility standards (visible focus, screen reader labeling).

**Phase 4: QA & Deployment (Weeks 11-12)**
- Run automated penetration testing on the cryptographic boundaries.
- Execute end-to-end tests for the 7-day grace period cron jobs.
- Final legal/compliance sign-off before production cutover and public disclosure.
