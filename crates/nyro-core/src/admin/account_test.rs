//! Read-only account-pool connectivity checks. These authenticate with the
//! non-generating status RPC; they neither spend chat quota nor claim that a
//! particular model is usable. A pool succeeds when any enabled account passes.
use super::*;
use futures::{StreamExt, stream};
use std::future::Future;

const ACCOUNT_TEST_CONCURRENCY: usize = 4;
const ACCOUNT_TEST_TIMEOUT: Duration = Duration::from_secs(15);

impl AdminService {
    pub(super) async fn test_devin_account_connections(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<TestResult> {
        let accounts = self
            .gw
            .storage
            .oauth_credentials()
            .list_accounts(&provider.id)
            .await?;
        let admin = self.clone();
        let provider = provider.clone();
        Ok(probe_accounts(accounts, move |account| {
            let admin = admin.clone();
            let provider = provider.clone();
            async move {
                let current = admin
                    .gw
                    .storage
                    .oauth_credentials()
                    .get_account(&provider.id, &account.account_id)
                    .await?;
                if !same_generation(current.as_ref(), &account) {
                    return Ok(ProbeOutcome::Changed);
                }
                let runtime = admin
                    .resolve_provider_account_runtime(&provider, &account)
                    .await?;
                let outcome = crate::provider::devin::check_connection(
                    &admin.gw,
                    &provider,
                    &runtime.access_token,
                )
                .await;
                // A removal, disable, or reauthorization while the RPC ran is
                // authoritative. Do not report old credentials as a current pass.
                let current = admin
                    .gw
                    .storage
                    .oauth_credentials()
                    .get_account(&provider.id, &account.account_id)
                    .await?;
                if !same_generation(current.as_ref(), &account) {
                    return Ok(ProbeOutcome::Changed);
                }
                outcome.map(|()| ProbeOutcome::Connected)
            }
        })
        .await)
    }
}

fn same_generation(current: Option<&OAuthCredential>, tested: &OAuthCredential) -> bool {
    current.is_some_and(|current| {
        current.status_version == tested.status_version
            && current.is_enabled
            && current.status == "connected"
    })
}

enum ProbeOutcome {
    Connected,
    Changed,
}

/// The injected operation is the production account/runtime/status sequence;
/// tests replace only that external effect, not the pool aggregation rules.
async fn probe_accounts<F, Fut>(accounts: Vec<OAuthCredential>, probe: F) -> TestResult
where
    F: Fn(OAuthCredential) -> Fut,
    Fut: Future<Output = anyhow::Result<ProbeOutcome>>,
{
    let started = Instant::now();
    let mut results = stream::iter(accounts.into_iter().enumerate().map(|(index, account)| {
        let probe = &probe;
        async move {
            let started = Instant::now();
            let mut result = AccountTestResult {
                account_id: account.account_id.clone(),
                label: account.label.clone(),
                success: false,
                latency_ms: 0,
                error: None,
                skipped_reason: None,
                tested_at: Utc::now().to_rfc3339(),
            };
            if account.status == "disconnected" {
                result.skipped_reason = Some("disconnected".into());
            } else if !account.is_enabled {
                result.skipped_reason = Some("disabled".into());
            } else if account.status != "connected" || account.access_token.trim().is_empty() {
                result.error = Some("Devin account is not authorized; sign in again".into());
            } else {
                match tokio::time::timeout(ACCOUNT_TEST_TIMEOUT, probe(account)).await {
                    Ok(Ok(ProbeOutcome::Connected)) => result.success = true,
                    Ok(Ok(ProbeOutcome::Changed)) => {
                        result.skipped_reason = Some("account_changed".into())
                    }
                    Ok(Err(error)) => result.error = Some(safe_probe_error(&error)),
                    Err(_) => result.error = Some("Devin connectivity check timed out".into()),
                }
            }
            result.latency_ms = started.elapsed().as_millis() as u64;
            (index, result)
        }
    }))
    .buffer_unordered(ACCOUNT_TEST_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    results.sort_by_key(|(index, _)| *index);
    let accounts: Vec<_> = results.into_iter().map(|(_, account)| account).collect();
    let success = accounts.iter().any(|account| account.success);
    let error = if success {
        None
    } else if accounts.is_empty() {
        Some("No Devin accounts are connected".into())
    } else if accounts.iter().all(|account| {
        matches!(
            account.skipped_reason.as_deref(),
            Some("disabled" | "disconnected")
        )
    }) {
        Some("No enabled Devin accounts".into())
    } else {
        Some("No Devin account passed connectivity check".into())
    };
    TestResult {
        success,
        latency_ms: started.elapsed().as_millis() as u64,
        model: None,
        error,
        endpoints: Vec::new(),
        accounts,
    }
}

fn safe_probe_error(error: &anyhow::Error) -> String {
    // Do not return arbitrary error chains or raw upstream diagnostics. Even
    // parse/transport errors can carry identifiers or request credentials.
    let text = error.to_string();
    if text.starts_with("Devin authentication failed (HTTP 401)")
        || text.starts_with("Devin authentication failed (HTTP 403)")
    {
        "Devin account authentication was rejected; sign in again".into()
    } else if text == "account is unavailable" || text == "account credential is missing" {
        "Devin account is not authorized; sign in again".into()
    } else {
        "Devin connectivity check failed; verify the account and proxy/network settings".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn account(id: &str) -> OAuthCredential {
        serde_json::from_value(serde_json::json!({
            "account_id":id,"provider_id":"pool","label":format!("Account {id}"),"is_enabled":true,"priority":0,
            "account_state":"{}","driver_key":"devin","scheme":"oauth_auth_code_pkce","access_token":"SECRET-fixture",
            "refresh_token":null,"expires_at":null,"resource_url":null,"subject_id":null,
            "scopes":"[]","meta":"{}","status":"connected","status_version":1,
            "last_error":null,"last_refresh_at":null,"created_at":"","updated_at":""
        })).unwrap()
    }

    #[tokio::test]
    async fn one_success_makes_pool_connected_without_hiding_failed_accounts() {
        let mut disabled = account("disabled");
        disabled.is_enabled = false;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let result = probe_accounts(
            vec![account("bad"), disabled, account("good")],
            move |account| {
                count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if account.account_id == "bad" {
                        anyhow::bail!("opaque SECRET-fixture upstream body");
                    }
                    Ok(ProbeOutcome::Connected)
                }
            },
        )
        .await;
        assert!(result.success);
        assert!(result.error.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            result
                .accounts
                .iter()
                .map(|a| a.account_id.as_str())
                .collect::<Vec<_>>(),
            vec!["bad", "disabled", "good"]
        );
        assert!(!result.accounts[0].success && result.accounts[0].error.is_some());
        assert_eq!(
            result.accounts[1].skipped_reason.as_deref(),
            Some("disabled")
        );
        assert!(result.accounts[2].success);
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(
            !serialized.contains("SECRET-fixture")
                && !serialized.contains("access_token")
                && !serialized.contains("opaque")
        );
    }

    #[tokio::test]
    async fn empty_disabled_and_missing_credentials_do_not_claim_success_or_probe() {
        let mut disabled = account("disabled");
        disabled.is_enabled = false;
        let mut missing = account("missing");
        missing.access_token.clear();
        for rows in [vec![], vec![disabled], vec![missing]] {
            let result =
                probe_accounts(rows, |_| async { panic!("unexpected network probe") }).await;
            assert!(!result.success);
            assert!(result.error.is_some());
        }
    }

    #[tokio::test]
    async fn all_failed_and_changed_accounts_do_not_claim_success() {
        let result = probe_accounts(
            vec![account("failed"), account("changed")],
            |a| async move {
                if a.account_id == "changed" {
                    Ok(ProbeOutcome::Changed)
                } else {
                    anyhow::bail!("Devin authentication failed (HTTP 401); private detail")
                }
            },
        )
        .await;
        assert!(!result.success);
        assert_eq!(
            result.accounts[1].skipped_reason.as_deref(),
            Some("account_changed")
        );
        assert_eq!(
            result.accounts[0].error.as_deref(),
            Some("Devin account authentication was rejected; sign in again")
        );
    }

    #[tokio::test]
    async fn quota_exhaustion_does_not_prevent_non_generating_connectivity_check() {
        let mut exhausted = account("quota");
        exhausted.account_state = r#"{"hard_quota":true,"auth_invalid":true}"#.into();
        let result =
            probe_accounts(vec![exhausted], |_| async { Ok(ProbeOutcome::Connected) }).await;
        assert!(result.success);
        assert_eq!(result.accounts.len(), 1);
    }

    #[tokio::test]
    async fn concurrency_is_bounded_and_output_order_is_stable() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (count, highest) = (active.clone(), peak.clone());
        let result = probe_accounts(
            (0..9).map(|i| account(&i.to_string())).collect(),
            move |_| {
                let (active, peak) = (count.clone(), highest.clone());
                async move {
                    let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(ProbeOutcome::Connected)
                }
            },
        )
        .await;
        assert!(result.success);
        assert_eq!(result.accounts.len(), 9);
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= ACCOUNT_TEST_CONCURRENCY);
        assert_eq!(result.accounts[8].account_id, "8");
    }

    #[test]
    fn changed_generation_and_disabled_accounts_invalidate_old_passes() {
        let tested = account("a");
        assert!(same_generation(Some(&tested), &tested));
        let mut changed = tested.clone();
        changed.status_version += 1;
        assert!(!same_generation(Some(&changed), &tested));
        changed = tested.clone();
        changed.is_enabled = false;
        assert!(!same_generation(Some(&changed), &tested));
        assert!(!same_generation(None, &tested));
        let legacy: TestResult = serde_json::from_value(
            serde_json::json!({"success":true,"latency_ms":1,"model":null,"error":null}),
        )
        .unwrap();
        assert!(legacy.accounts.is_empty());
    }
}
