//! Upstream side of nyro→nyro usage forwarding (`GET /v1/nyro/usage`).
//!
//! A downstream Nyro whose provider is the Nyro preset (`nyro-gateway`) reads
//! this instance's upstream usage through the proxy port, with the same key it
//! routes with. The report is scoped to what that key can do:
//!
//! - only providers behind routes the key can route to (the `/v1/models`
//!   visibility rule), so a bound key never learns about unrelated accounts;
//! - money figures (`balances`, `spends`) only for the gateway key and
//!   privileged API keys. A regular key still sees quota windows and pause
//!   state, which decide whether its routes keep working;
//! - account-pool details (per-account identities) are never forwarded.
//!
//! Chain guard: this endpoint never queries its own `nyro-gateway` providers.
//! They are reported as `unsupported`, so two instances pointing at each
//! other cannot recurse. Snapshots come from a short-lived cache, so many
//! downstream instances polling one remote do not multiply vendor quota calls.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::RwLock;

use futures::stream::{self, StreamExt};

use super::usage::{
    LIST_USAGE_CONCURRENCY, ProviderUsage, ProviderUsageQueryStatus, provider_usage_monitorable,
};
use super::*;

/// Reuse window for forwarded snapshots. Downstream monitors poll every five
/// minutes, so one remote serves any number of them with at most one vendor
/// query per provider per window.
pub(crate) const FORWARDED_USAGE_MAX_AGE: Duration = Duration::from_secs(60);
const MAX_FORWARDED_ERROR_CHARS: usize = 300;

/// Last successful full usage snapshot per provider.
#[derive(Default)]
pub(crate) struct UsageSnapshotCache {
    entries: RwLock<HashMap<String, (Instant, ProviderUsage)>>,
}

impl UsageSnapshotCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn store(&self, usage: &ProviderUsage) {
        self.entries
            .write()
            .unwrap()
            .insert(usage.provider_id.clone(), (Instant::now(), usage.clone()));
    }

    pub(crate) fn fresh(&self, provider_id: &str, max_age: Duration) -> Option<ProviderUsage> {
        self.entries
            .read()
            .unwrap()
            .get(provider_id)
            .filter(|(stored_at, _)| stored_at.elapsed() <= max_age)
            .map(|(_, usage)| usage.clone())
    }

    pub(crate) fn invalidate(&self, provider_id: &str) {
        self.entries.write().unwrap().remove(provider_id);
    }
}

/// Which routes a proxy caller may use, shared by `/v1/models` and usage
/// forwarding so both apply one visibility rule.
#[derive(Debug, Clone, Default)]
pub struct RouteAccess {
    /// Gateway key or privileged API key: every route.
    pub privileged: bool,
    /// Routes bound to a regular API key.
    pub bound_route_ids: HashSet<String>,
}

impl RouteAccess {
    pub fn privileged() -> Self {
        Self {
            privileged: true,
            bound_route_ids: HashSet::new(),
        }
    }

    /// Open routes are visible to everyone; auth-enabled ones need a binding.
    pub fn can_route(&self, model: &Model) -> bool {
        self.privileged || !model.enable_auth || self.bound_route_ids.contains(&model.id)
    }
}

/// Forwarded usage of one upstream provider behind the caller's routes.
#[derive(Debug, Clone, Serialize)]
pub struct ForwardedProviderUsage {
    pub id: String,
    pub name: String,
    /// Preset or vendor key, for the downstream icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Route names (on this instance) the provider serves for the caller.
    pub routes: Vec<String>,
    pub status: ProviderUsageQueryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderUsage>,
}

/// `GET /v1/nyro/usage` response body.
#[derive(Debug, Clone, Serialize)]
pub struct ForwardedUsageReport {
    /// Always `nyro.usage`.
    pub object: &'static str,
    pub version: u32,
    pub generated_at: String,
    /// Usage fields withheld from this caller.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub redacted: Vec<&'static str>,
    pub providers: Vec<ForwardedProviderUsage>,
}

fn truncate_error(error: &str) -> String {
    let mut text: String = error.chars().take(MAX_FORWARDED_ERROR_CHARS).collect();
    if error.chars().count() > MAX_FORWARDED_ERROR_CHARS {
        text.push('…');
    }
    text
}

impl AdminService {
    /// Usage of every enabled provider behind the routes `access` can use.
    pub async fn forwarded_provider_usage(
        &self,
        access: &RouteAccess,
    ) -> anyhow::Result<ForwardedUsageReport> {
        let mut routes_by_provider: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        {
            let cache = self.gw.model_cache.read().await;
            for model in cache.models.iter().filter(|model| access.can_route(model)) {
                let route = model.name.trim();
                if route.is_empty() {
                    continue;
                }
                let provider_ids = model
                    .targets
                    .iter()
                    .map(|target| target.provider_id.as_str())
                    .chain(std::iter::once(model.target_provider.as_str()));
                for provider_id in provider_ids.map(str::trim).filter(|id| !id.is_empty()) {
                    routes_by_provider
                        .entry(provider_id.to_string())
                        .or_default()
                        .insert(route.to_string());
                }
            }
        }

        let candidates = self
            .list_providers()
            .await?
            .into_iter()
            .filter(|provider| provider.is_enabled)
            .filter_map(|provider| {
                routes_by_provider
                    .remove(&provider.id)
                    .map(|routes| (provider, routes))
            })
            .collect::<Vec<_>>();
        let include_money = access.privileged;
        let mut providers = stream::iter(candidates)
            .map(|(provider, routes)| {
                let admin = self.clone();
                async move {
                    admin
                        .forwarded_provider_entry(provider, routes, include_money)
                        .await
                }
            })
            .buffer_unordered(LIST_USAGE_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        providers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));

        Ok(ForwardedUsageReport {
            object: crate::provider::nyro_gateway::usage::REPORT_OBJECT,
            version: 1,
            generated_at: Utc::now().to_rfc3339(),
            redacted: if include_money {
                Vec::new()
            } else {
                vec!["balances", "spends"]
            },
            providers,
        })
    }

    async fn forwarded_provider_entry(
        &self,
        provider: Provider,
        routes: BTreeSet<String>,
        include_money: bool,
    ) -> ForwardedProviderUsage {
        let mut entry = ForwardedProviderUsage {
            id: provider.id.clone(),
            name: provider.name.clone(),
            vendor: [provider.preset_key.as_deref(), provider.vendor.as_deref()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .find(|value| !value.is_empty())
                .map(str::to_ascii_lowercase),
            routes: routes.into_iter().collect(),
            status: ProviderUsageQueryStatus::Unsupported,
            error: None,
            usage: None,
        };
        // Chain guard: never call out to another Nyro from this endpoint.
        if crate::provider::nyro_gateway::is_nyro_gateway(&provider)
            || !provider_usage_monitorable(self, &provider).await
        {
            return entry;
        }
        let usage = match self
            .gw
            .usage_snapshots
            .fresh(&provider.id, FORWARDED_USAGE_MAX_AGE)
        {
            Some(usage) => Ok(usage),
            None => self.get_provider_usage(&provider.id).await,
        };
        match usage {
            Ok(mut usage) => {
                usage.account_pool = None;
                usage.upstream = None;
                if !include_money {
                    usage.balances.clear();
                    usage.spends.clear();
                }
                entry.status = ProviderUsageQueryStatus::Ok;
                entry.usage = Some(usage);
            }
            Err(error) => {
                entry.status = ProviderUsageQueryStatus::Error;
                entry.error = Some(truncate_error(&error.to_string()));
            }
        }
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use crate::router::quota::{ProviderScheduling, ProviderSchedulingStatus};

    async fn gateway(dir: &tempfile::TempDir) -> Gateway {
        let config = GatewayConfig {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        Gateway::new(config).await.expect("gateway").0
    }

    async fn provider(
        gw: &Gateway,
        name: &str,
        vendor: Option<&str>,
        base_url: &str,
        enabled: bool,
    ) -> String {
        let id = gw
            .admin()
            .create_provider(CreateProvider {
                keys: Vec::new(),
                name: name.into(),
                vendor: vendor.map(str::to_string),
                protocol: "openai-compatible".into(),
                base_url: base_url.into(),
                protocol_mode: "fixed".into(),
                protocol_endpoints: Vec::new(),
                preset_key: vendor.map(str::to_string),
                channel: None,
                models_source: None,
                static_models: None,
                api_key: "sk-test".into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
                fast_mode: false,
            })
            .await
            .expect("create provider")
            .id;
        if !enabled {
            gw.admin()
                .update_provider(
                    &id,
                    UpdateProvider {
                        is_enabled: Some(false),
                        ..Default::default()
                    },
                )
                .await
                .expect("disable provider");
        }
        id
    }

    async fn route(gw: &Gateway, name: &str, provider: &str, enable_auth: bool) -> String {
        gw.admin()
            .create_model(CreateModel {
                name: name.into(),
                balance: Some("priority".into()),
                target_provider: provider.into(),
                target_model: "m".into(),
                targets: vec![],
                enable_auth: Some(enable_auth),
                enable_payload: Some(false),
                force_max_reasoning: None,
                vision_shim: None,
            })
            .await
            .expect("create route")
            .id
    }

    fn snapshot(provider_id: &str, paused: bool) -> ProviderUsage {
        ProviderUsage {
            provider_id: provider_id.into(),
            kind: "deepseek_balance".into(),
            site: "global".into(),
            level: Some("pro".into()),
            tiers: vec![ProviderUsageTier {
                name: "weekly_limit".into(),
                used_percent: 42.0,
                resets_at: None,
            }],
            balances: vec![ProviderUsageBalance {
                currency: "CNY".into(),
                total: 12.5,
                granted: 0.0,
                topped_up: 12.5,
            }],
            spends: vec![super::super::usage::ProviderUsageSpend {
                name: "today".into(),
                amount: 1.0,
                currency: "CNY".into(),
            }],
            is_available: Some(true),
            scheduling: ProviderScheduling {
                status: if paused {
                    ProviderSchedulingStatus::QuotaExhausted
                } else {
                    ProviderSchedulingStatus::Eligible
                },
                ..Default::default()
            },
            queried_at: Utc::now().to_rfc3339(),
            account_pool: None,
            upstream: Some(serde_json::json!({"providers": []})),
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        gw: Gateway,
        deepseek: String,
        bound_route: String,
    }

    /// DeepSeek (usage backend, cached snapshot) behind an auth route; a
    /// custom relay (no backend) behind an open route; a chained Nyro pointing
    /// at an unroutable address; a disabled provider; an unrelated route.
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let gw = gateway(&dir).await;
        let deepseek = provider(
            &gw,
            "DeepSeek",
            Some("deepseek"),
            "https://api.deepseek.com",
            true,
        )
        .await;
        let relay = provider(&gw, "Relay", None, "https://relay.example/v1", true).await;
        let chained = provider(
            &gw,
            "Edge Nyro",
            Some("nyro-gateway"),
            "http://192.0.2.1:19530",
            true,
        )
        .await;
        let disabled = provider(
            &gw,
            "Off",
            Some("deepseek"),
            "https://api.deepseek.com",
            false,
        )
        .await;
        let other = provider(
            &gw,
            "Other",
            Some("deepseek"),
            "https://api.deepseek.com",
            true,
        )
        .await;
        let bound_route = route(&gw, "chat", &deepseek, true).await;
        route(&gw, "relay", &relay, false).await;
        route(&gw, "edge", &chained, false).await;
        route(&gw, "off", &disabled, false).await;
        route(&gw, "secret", &other, true).await;
        // No network in tests: serve the DeepSeek accounts from the cache.
        gw.usage_snapshots.store(&snapshot(&deepseek, false));
        gw.usage_snapshots.store(&snapshot(&other, false));
        Fixture {
            _dir: dir,
            gw,
            deepseek,
            bound_route,
        }
    }

    fn names(report: &ForwardedUsageReport) -> Vec<&str> {
        report.providers.iter().map(|p| p.name.as_str()).collect()
    }

    #[tokio::test]
    async fn privileged_callers_see_every_route_with_money_but_no_pool_or_nested_data() {
        let f = fixture().await;
        let report =
            f.gw.admin()
                .forwarded_provider_usage(&RouteAccess::privileged())
                .await
                .unwrap();
        assert_eq!(report.object, "nyro.usage");
        assert!(report.redacted.is_empty());
        // Disabled providers are not serving traffic and are left out.
        assert_eq!(names(&report), ["DeepSeek", "Edge Nyro", "Other", "Relay"]);

        let deepseek = &report.providers[0];
        assert_eq!(deepseek.id, f.deepseek);
        assert_eq!(deepseek.vendor.as_deref(), Some("deepseek"));
        assert_eq!(deepseek.routes, ["chat"]);
        assert_eq!(deepseek.status, ProviderUsageQueryStatus::Ok);
        let usage = deepseek.usage.as_ref().unwrap();
        assert_eq!(usage.balances.len(), 1);
        assert_eq!(usage.spends.len(), 1);
        assert!(usage.upstream.is_none(), "forwarding is one hop deep");
        assert!(usage.account_pool.is_none());

        // The chained Nyro is reported, never queried (it points at TEST-NET).
        let edge = &report.providers[1];
        assert_eq!(edge.status, ProviderUsageQueryStatus::Unsupported);
        assert!(edge.usage.is_none());
        assert_eq!(
            report.providers[3].status,
            ProviderUsageQueryStatus::Unsupported
        );
    }

    #[tokio::test]
    async fn regular_keys_see_their_routes_without_money() {
        let f = fixture().await;
        let access = RouteAccess {
            privileged: false,
            bound_route_ids: HashSet::from([f.bound_route.clone()]),
        };
        let report =
            f.gw.admin()
                .forwarded_provider_usage(&access)
                .await
                .unwrap();
        assert_eq!(report.redacted, ["balances", "spends"]);
        // Bound auth route + open routes; the unbound auth route stays hidden.
        assert_eq!(names(&report), ["DeepSeek", "Edge Nyro", "Relay"]);
        let usage = report.providers[0].usage.as_ref().unwrap();
        assert!(usage.balances.is_empty() && usage.spends.is_empty());
        assert_eq!(usage.tiers.len(), 1, "quota windows stay visible");
        assert_eq!(usage.level.as_deref(), Some("pro"));

        let wire = serde_json::to_value(&report).unwrap();
        assert_eq!(
            wire["providers"][0]["usage"]["balances"],
            serde_json::json!([])
        );
        assert!(wire["providers"][0]["usage"].get("upstream").is_none());
    }

    #[tokio::test]
    async fn anonymous_route_access_is_limited_to_open_routes() {
        let f = fixture().await;
        let report =
            f.gw.admin()
                .forwarded_provider_usage(&RouteAccess::default())
                .await
                .unwrap();
        assert_eq!(names(&report), ["Edge Nyro", "Relay"]);
    }

    #[test]
    fn snapshot_cache_expires_and_invalidates() {
        let cache = UsageSnapshotCache::new();
        cache.store(&snapshot("p", true));
        assert!(cache.fresh("p", Duration::from_secs(60)).is_some());
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.fresh("p", Duration::from_millis(1)).is_none());
        cache.invalidate("p");
        assert!(cache.fresh("p", Duration::from_secs(60)).is_none());
    }

    #[test]
    fn forwarded_errors_are_bounded() {
        let long = "x".repeat(MAX_FORWARDED_ERROR_CHARS + 10);
        let text = truncate_error(&long);
        assert_eq!(text.chars().count(), MAX_FORWARDED_ERROR_CHARS + 1);
        assert_eq!(truncate_error("short"), "short");
    }
}
