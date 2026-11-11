//! Read-only account-pool connectivity checks. These authenticate with the
//! non-generating status RPC; they neither spend chat quota nor claim that a
//! particular model is usable. A pool succeeds when any enabled account passes.
use super::*;
use crate::provider::anthropic::claude_code::usage_throttle::Cooldown;
use futures::{StreamExt, stream};
use std::future::Future;

const ACCOUNT_TEST_CONCURRENCY: usize = 4;
const ACCOUNT_TEST_TIMEOUT: Duration = Duration::from_secs(15);

impl AdminService {
    pub(super) async fn test_pool_account_connections(
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
        let kiro = crate::provider::kiro::is_kiro(provider);
        let claude = is_claude_provider(provider);
        let name = if kiro {
            "Kiro"
        } else if claude {
            "Claude Code"
        } else {
            "Devin"
        };
        let provider = provider.clone();
        let mut result = probe_accounts(accounts, move |account| {
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
                // Admission deferral is not failed connectivity. Avoid even token
                // refresh/runtime resolution while the usage probe is prohibited.
                if claude
                    && let Some(retry_at) = admin
                        .claude_usage_refresh_deadline(&provider, &account)
                        .await?
                    && retry_at > Utc::now().timestamp_millis()
                {
                    let current = admin
                        .gw
                        .storage
                        .oauth_credentials()
                        .get_account(&provider.id, &account.account_id)
                        .await?;
                    return Ok(if same_generation(current.as_ref(), &account) {
                        ProbeOutcome::Cooldown { retry_at }
                    } else {
                        ProbeOutcome::Changed
                    });
                }
                let account = if kiro {
                    admin.refresh_kiro_pool_account(&provider, &account).await?
                } else if claude {
                    admin
                        .refresh_claude_pool_account(&provider, &account)
                        .await?
                } else {
                    account
                };
                let runtime = admin
                    .resolve_provider_account_runtime(&provider, &account)
                    .await?;
                let outcome = if claude {
                    let client = admin
                        .gw
                        .http_client_for_provider(provider.use_proxy)
                        .await?;
                    crate::provider::anthropic::claude_code::usage::fetch_usage(
                        &admin.gw,
                        &client,
                        runtime
                            .binding
                            .base_url_override
                            .as_deref()
                            .unwrap_or(&provider.base_url),
                        &runtime.access_token,
                        &stored_credential_from_oauth(&account, "claude-code"),
                        crate::provider::account_pool::ClaudeAccountState::parse(
                            &account.account_state,
                        )
                        .observed_at,
                    )
                    .await
                    .map(|_| ())
                } else if kiro {
                    crate::provider::kiro::check_connection(
                        &admin.gw,
                        &provider,
                        runtime
                            .credential
                            .as_ref()
                            .context("account credential is missing")?,
                        &runtime.access_token,
                    )
                    .await
                } else {
                    crate::provider::devin::check_connection(
                        &admin.gw,
                        &provider,
                        &runtime.access_token,
                    )
                    .await
                };
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
                match outcome {
                    Err(error) if error.is::<Cooldown>() => Ok(ProbeOutcome::Cooldown {
                        retry_at: error.downcast_ref::<Cooldown>().unwrap().next_refresh_at,
                    }),
                    result => result.map(|()| ProbeOutcome::Connected),
                }
            }
        })
        .await;
        // Shared probe logic keeps its legacy Devin wording for compatibility.
        // Public Kiro results use their own vendor name, never upstream text.
        if kiro || claude {
            result.error = result.error.map(|e| e.replace("Devin", name));
            for account in &mut result.accounts {
                account.error = account.error.take().map(|e| e.replace("Devin", name));
            }
        }
        Ok(result)
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
    Cooldown { retry_at: i64 },
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
                retry_at: None,
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
                    Ok(Ok(ProbeOutcome::Cooldown { retry_at })) => {
                        result.skipped_reason = Some("usage_cooldown".into());
                        result.retry_at = Some(retry_at);
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
    let skipped_reason = (accounts
        .iter()
        .all(|account| account.skipped_reason.is_some())
        && accounts
            .iter()
            .any(|account| account.skipped_reason.as_deref() == Some("usage_cooldown")))
    .then(|| "usage_cooldown".to_owned());
    let error = if success || skipped_reason.is_some() {
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
        skipped_reason,
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
        || text.starts_with("Kiro authentication failed (HTTP 401)")
        || text.starts_with("Kiro authentication failed (HTTP 403)")
        || text.starts_with("Claude authentication failed (HTTP ")
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
    async fn cooldown_accounts_are_skipped_without_connectivity_errors_or_success() {
        let retry_at = 1_800_000_600_000;
        let mut disabled = account("disabled");
        disabled.is_enabled = false;
        let result = probe_accounts(vec![account("one"), disabled, account("two")], |_| async {
            Ok(ProbeOutcome::Cooldown { retry_at })
        })
        .await;
        assert!(!result.success);
        assert!(result.error.is_none());
        assert_eq!(result.skipped_reason.as_deref(), Some("usage_cooldown"));
        for index in [0, 2] {
            let row = &result.accounts[index];
            assert!(!row.success && row.error.is_none());
            assert_eq!(row.skipped_reason.as_deref(), Some("usage_cooldown"));
            assert_eq!(row.retry_at, Some(retry_at));
        }
        assert_eq!(
            result.accounts[1].skipped_reason.as_deref(),
            Some("disabled")
        );
        assert!(result.accounts[1].retry_at.is_none());
        let wire = serde_json::to_value(&result).unwrap();
        assert_eq!(wire["skipped_reason"], "usage_cooldown");
        assert_eq!(wire["accounts"][0]["retry_at"], retry_at);
        let decoded: TestResult = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.accounts[2].retry_at, Some(retry_at));
    }

    #[tokio::test]
    async fn cooldown_does_not_mask_real_failures_or_fabricate_passes_in_mixed_pool() {
        for succeeds in [false, true] {
            let result = probe_accounts(
                vec![account("cooling"), account("tested")],
                |a| async move {
                    if a.account_id == "cooling" {
                        Ok(ProbeOutcome::Cooldown {
                            retry_at: 1_800_000_600_000,
                        })
                    } else if succeeds {
                        Ok(ProbeOutcome::Connected)
                    } else {
                        anyhow::bail!("Claude authentication failed (HTTP 401); SECRET")
                    }
                },
            )
            .await;
            assert_eq!(result.success, succeeds);
            assert!(result.skipped_reason.is_none());
            assert_eq!(result.error.is_none(), succeeds);
            assert_eq!(
                result.accounts[0].skipped_reason.as_deref(),
                Some("usage_cooldown")
            );
            assert!(result.accounts[0].error.is_none());
            assert!(result.accounts[1].skipped_reason.is_none());
            assert!(result.accounts[1].retry_at.is_none());
            if !succeeds {
                assert_eq!(
                    result.accounts[1].error.as_deref(),
                    Some("Devin account authentication was rejected; sign in again")
                );
                assert!(!serde_json::to_string(&result).unwrap().contains("SECRET"));
            }
        }
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
        assert!(legacy.skipped_reason.is_none());
        let legacy_account: AccountTestResult = serde_json::from_value(serde_json::json!({
            "account_id":"a", "label":"A", "success":true, "latency_ms":1,
            "error":null, "skipped_reason":null, "tested_at":"2030-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(legacy_account.retry_at.is_none());
    }
}
