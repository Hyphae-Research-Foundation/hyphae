<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native product error model v1

| Code | Category | Retry default |
| --- | --- | --- |
| `data_directory_exists` | `conflict` | `never` |
| `data_directory_locked` | `unavailable` | `after-backoff` |
| `invalid_data_directory` | `corruption` | `after-recovery` |
| `format2_directory` | `invalid-request` | `never` |
| `catalog_object_not_found` | `not-found` | `never` |
| `sql_invalid_syntax` | `invalid-request` | `never` |
| `sql_parameter_mismatch` | `invalid-request` | `never` |
| `sql_catalog_changed` | `conflict` | `new-snapshot` |
| `sql_foreign_prepared` | `conflict` | `never` |
| `sql_unknown_object` | `not-found` | `never` |
| `sql_invalid_value` | `invalid-request` | `never` |
| `sql_no_access_path` | `invalid-request` | `never` |
| `sql_unique_violation` | `conflict` | `never` |
| `sql_check_violation` | `conflict` | `never` |
| `sql_foreign_key_violation` | `conflict` | `never` |
| `write_conflict` | `conflict` | `new-snapshot` |
| `object_not_found` | `not-found` | `never` |
| `limit_exceeded` | `limit` | `never` |
| `corruption` | `corruption` | `after-recovery` |
| `io` | `io` | `failure-dependent` |
| `internal` | `internal` | `never` |
| `invalid_request` | `invalid-request` | `never` |
| `catalog_conflict` | `conflict` | `new-snapshot` |
| `deadline_exceeded` | `deadline` | `same-request` |
| `cancelled` | `cancelled` | `same-request` |
| `authorization_denied` | `authorization` | `never` |
| `unavailable` | `unavailable` | `after-backoff` |
| `unknown_commit` | `unavailable` | `unknown-commit` |
| `backup_invalid` | `corruption` | `after-recovery` |
| `idempotency_conflict` | `conflict` | `never` |
| `secret_delivery_consumed` | `conflict` | `never` |
| `confirmation_digest_mismatch` | `authorization` | `never` |
| `upgrade_required` | `conflict` | `after-recovery` |
