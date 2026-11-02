# GDPR Test Plan — Login Feature

## 1. Data Minimization Tests

### AC-1.1: Form has exactly email + password fields
- **Goal:** Verify the login form only requests necessary credentials.
- **Method:** Inspect UI component props and HTML DOM. Ensure no extra `input` nodes exist in the form container.
- **Expected:** The rendered form contains exactly two inputs: `type="email"` and `type="password"`.

### AC-1.2: Registration rejects PII fields
- **Goal:** Verify backend does not accept non-essential Personal Identifiable Information during login or registration.
- **Method:** Inspect API payload schemas (Zod/Yup) and DB insertion logic. Attempt to POST request with extra PII fields (e.g., SSN, Date of Birth).
- **Expected:** API responds with `400 Bad Request` for unauthorized fields; these fields are not persisted in the database.

### AC-1.3: API payload inspection
- **Goal:** Verify outgoing/incoming payloads contain only minimal data points.
- **Method:** Inspect request bodies during E2E runs using logging or interceptors.
- **Expected:** Request body contains only `email` and `password`. No IP addresses, device fingerprints, or behavioral telemetry (e.g., mouse movements) are encoded in the login payload.

## 2. Encryption Tests

### AC-3.1: DB query returns Bcrypt/Argon2 hashes
- **Goal:** Verify credentials are stored using secure hashing algorithms.
- **Method:** Execute direct database queries against the `users` table immediately after registration.
- **Expected:** The `password_hash` column contains a string starting with `$2b$` (bcrypt) or `$argon2id$` (Argon2), not plaintext or weak hashes (MD5/SHA1).

### AC-3.2: Application logs never print secrets
- **Goal:** Prevent credential leakage via logging systems.
- **Method:** Run the application through E2E tests while capturing `stdout` and log files. Search for patterns matching email addresses sent during login or raw password strings.
- **Expected:** Log outputs are sanitized; no sensitive credentials are visible in logs, stdout, or stderr.

### AC-3.3: DB dump contains only encrypted values
- **Goal:** Ensure database backups and dumps protect user data at rest.
- **Method:** Generate a SQL/JSON dump of the auth table. Inspect content for plaintext passwords.
- **Expected:** Dump files contain only hashed password values and necessary account identifiers (email). Plaintext passwords are absent.

## 3. Consent Flow Tests

### AC-5.1: UI renders 3 independent toggles
- **Goal:** Verify granular consent options.
- **Method:** Run the login E2E test suite. Inspect the component tree for distinct toggle controls.
- **Expected:** The interface displays exactly three independent switch/toggle components (e.g., Login, Marketing, Analytics).

### AC-5.2: No Accept All button
- **Goal:** Ensure no "one-click" mass consent mechanisms exist.
- **Method:** Visual check or text search on the login component.
- **Expected:** There is no UI element labeled "Accept All", "Agree to all", or similar bulk-acceptance action.

### AC-5.3: Selective save works
- **Goal:** Verify consent state is correctly persisted based on individual toggles.
- **Method:** E2E test interaction sequence:
    1. Toggle "Marketing ON", "Login ON".
    2. Submit registration.
    3. Inspect the resulting `consent_history` or profile record.
- **Expected:** The backend saves exactly the selected toggles; other defaults are respected but not forced.

### AC-6.1: Zero pre-ticked checkboxes
- **Goal:** Verify no implied consent mechanisms (pre-ticking).
- **Method:** Load an unauthenticated login/registration form. Query initial state of consent toggle nodes.
- **Expected:** All boolean flags in the UI default to `false` (unchecked) upon load.

### AC-7.1: Append-only audit log
- **Goal:** Ensure consent changes are recorded immutably for auditability.
- **Method:** Perform a sequence of consent updates (e.g., change Marketing preference). Inspect DB logs or a specific audit table.
- **Expected:** A new record is created in the `consent_audit_log` table for each distinct action. No existing records are overwritten or deleted (Append-only pattern verified).

## 4. Right-to-Be-Forgotten Tests

### AC-8.1: POST /auth/forgive requires password re-auth
- **Goal:** Verify account deletion is gated by identity proof.
- **Method:** Intercept the request to `/auth/forgive` (or equivalent endpoint) in an E2E flow without a current session/password confirmation.
- **Expected:** API returns `401 Unauthorized` or `403 Forbidden`; no "delete" action occurs until valid credentials are provided again in the same flow.

### AC-8.2: Session tokens revoked immediately
- **Goal:** Ensure active sessions are terminated upon deletion request.
- **Method:** Trigger the delete flow while a backend E2E worker holds an active JWT or session cookie. Attempt to execute a protected API call immediately after deletion.
- **Expected:** The subsequent API call returns `401 Unauthorized` because the token is blacklisted or revoked in the database instantly upon "forgive" completion.

### AC-8.3: DB record deleted within 24 hours
- **Goal:** Verify timely data erasure (GDPR compliance window).
- **Method:** Execute a delete request, then inspect the database immediately and check for cron job schedules or async processing queues responsible for final purging.
- **Expected:** The user profile row is flagged for deletion immediately; full cryptographic deletion/purging completes within `24h`. For immediate verification tests, confirm the state change to "deleted" status.

### AC-9.1: Confirmation banner present
- **Goal:** Prevent accidental account destruction.
- **Method:** UI inspection of the delete flow step.
- **Expected:** A modal or inline element exists that requires explicit typing of a confirmation string (e.g., "DELETE MY ACCOUNT") before submission.

### AC-9.2: 7-day grace period email
- **Goal:** Verify notification protocols exist during the grace period.
- **Method:** Trigger account deletion, then check the `emails_sent` table or email service logs for a transactional message.
- **Expected:** An email is sent to the user's address informing them of pending deletion and the option to reactivate within 7 days.

### AC-10.1: Anonymize button in settings
- **Goal:** Verify self-service anonymity features outside of full deletion flows.
- **Method:** Navigate to User Settings page. Search for "Anonymize" or "Data removal" functionality.
- **Expected:** A functional "Anonymize Account" option exists that transforms the profile by wiping name/PII but preserving transaction history, distinct from the full delete flow.

## 5. Verification Commands

### TLS Scanning
*   Verify server configuration using sslyze:
    ```bash
    sslyze --tlsv13 https://target-server.com --json_output=report.json
    # Check report for supportedCipherSuites containing only ECDHE-RSA-AES256-GCM-SHA384 or AESGCM variants
    ```

### DB Row Inspection Queries
*   Verify encryption at rest:
    ```sql
    SELECT email, password_hash FROM users LIMIT 1;
    -- Ensure 'password_hash' matches $argon2$id=... or $2b$ patterns
    ```
*   Verify consent logs (Append-only check):
    ```sql
    SELECT * FROM audit_logs WHERE user_id = '<test_user_id>' AND type='CONSENT_CHANGE';
    ```

### E2E Test Commands
*   Run full auth suite:
    ```bash
    npm run test:e2e -- --grep "GDPR|Auth|Privacy"
    ```
*   Visual regression (Consent UI):
    ```bash
    npx playwright test tests/gdpr/consent.spec.ts --headed --screenshot=on
    ```
