# GDPR Data Architecture — Login Feature

## 1. Encryption Key Management
- KMS selection: AWS KMS or HashiCorp Vault
- Key rotation: annual max, versioned key IDs
- Key storage: never in env vars or source control

## 2. Data Minimization Architecture
- Minimal login payload: email + password only
- PII decoupled into optional profile-update flow
- Default refusal enforcement

## 3. Cascade Erasure Data Flow
- POST /auth/forgive endpoint
- 24-hour active DB deletion
- 30-day backup purge
- Anonymization fallback for legal holds

## 4. References
- DISCUSSION-001 at tasks/discussions/DISCUSSION-001-gdpr-compliance-review.md
- GDPR Articles 5, 7, 17, 25, 32
