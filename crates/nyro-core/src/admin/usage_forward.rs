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
//! - account-pool details (per-account identities) and local retained-log API
//!   price statistics are admin-only and never forwarded, even to privileged keys.
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
use super::usage_publication::{bucket_tiers_conflict, bucket_usage, effective_bucket_scheduling};
use super::*;
use crate::provider::nyro_gateway::usage::public_usage::PublicProviderUsage;
use crate::quota_buckets::opaque_quota_id;

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
        let mut usage = usage.clone();
        // This cache serves proxy forwarding, not admin statistics. Never retain
        // local log prices here, even if an enriched admin snapshot is supplied.
        usage.api_price = None;
        usage.account_pool = None;
        usage.upstream = None;
        for bucket in &mut usage.quota_buckets {
            bucket.api_price = None;
        }
        self.entries
            .write()
            .unwrap()
            .insert(usage.provider_id.clone(), (Instant::now(), usage));
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
    pub usage: Option<PublicProviderUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_entry: Option<super::usage_publication::PublishedUsageEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForwardedRouteScheduling {
    pub route: String,
    pub scheduling: ProviderScheduling,
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
    pub capabilities: Vec<&'static str>,
    pub route_scheduling: Vec<ForwardedRouteScheduling>,
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
        // Snapshot the visible routes first so the cache lock is not held
        // across backend-store queries.
        let visible_routes = {
            let cache = self.gw.model_cache.read().await;
            cache
                .models
                .iter()
                .filter(|model| {
                    model.is_enabled && access.can_route(model) && !model.name.trim().is_empty()
                })
                .map(|model| {
                    (
                        model.id.clone(),
                        model.name.trim().to_string(),
                        model.balance.clone(),
                        (model.target_provider.clone(), model.target_model.clone()),
                        model.targets.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut routes_by_provider: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> =
            BTreeMap::new();
        for (model_id, route, balance, primary, cached_targets) in visible_routes {
            // The SQL route snapshot carries only the primary provider; every
            // other target (weighted peers, fallbacks) lives in the backend
            // store. Resolve them the way the dispatcher does, so a provider
            // that only backs a route as fallback is still reported.
            let stored_targets = match self.gw.storage.model_backends() {
                Some(store) => match store.list_backends_by_model(&model_id).await {
                    Ok(backends) => backends,
                    Err(error) => {
                        tracing::warn!(%error, route, "usage forwarding could not load route backends");
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };
            // Stored rows are authoritative when present; do not accidentally
            // retain a stale cached primary as another healthy alternative.
            let targets = if !stored_targets.is_empty() {
                stored_targets
            } else if !cached_targets.is_empty() {
                cached_targets
            } else if !primary.0.trim().is_empty() {
                vec![ModelBackend {
                    id: String::new(),
                    model_id: model_id.clone(),
                    provider_id: primary.0,
                    model: primary.1,
                    weight: 100,
                    priority: 1,
                    is_fallback: false,
                    created_at: String::new(),
                }]
            } else {
                Vec::new()
            };
            // Mirror selector's static eligibility before exposing an OR
            // alternative. Priority/latency use weight only for ordering (or
            // ignore it); weighted/usage omit nonpositive regular rows. Fallback
            // rows bypass those strategies and remain eligible at any weight.
            for target in targets
                .into_iter()
                .filter(|target| forwarded_target_enabled(&balance, target))
            {
                let provider_id = target.provider_id.trim();
                let backend_model = target.model;
                if provider_id.is_empty() {
                    continue;
                }
                let backend_model = if backend_model.is_empty() || backend_model == "*" {
                    route.clone()
                } else {
                    backend_model
                };
                routes_by_provider
                    .entry(provider_id.to_string())
                    .or_default()
                    .entry(route.clone())
                    .or_default()
                    .insert(backend_model);
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
        let entries = stream::iter(candidates)
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
        let mut providers = Vec::new();
        let mut route_choices: BTreeMap<String, Vec<Option<ProviderScheduling>>> = BTreeMap::new();
        for (rows, states) in entries {
            providers.extend(rows);
            for (route, states) in states {
                route_choices.entry(route).or_default().extend(states);
            }
        }
        providers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        let route_scheduling = route_choices
            .into_iter()
            .map(|(route, choices)| ForwardedRouteScheduling {
                route,
                scheduling: alternative_scheduling(&choices),
            })
            .collect();

        Ok(ForwardedUsageReport {
            object: crate::provider::nyro_gateway::usage::REPORT_OBJECT,
            version: 2,
            generated_at: Utc::now().to_rfc3339(),
            redacted: if include_money {
                Vec::new()
            } else {
                vec!["balances", "spends"]
            },
            providers,
            capabilities: vec!["quota_bucket_projection", "route_scheduling"],
            route_scheduling,
        })
    }

    async fn forwarded_provider_entry(
        &self,
        provider: Provider,
        routes: BTreeMap<String, BTreeSet<String>>,
        include_money: bool,
    ) -> (
        Vec<ForwardedProviderUsage>,
        BTreeMap<String, Vec<Option<ProviderScheduling>>>,
    ) {
        let mut entry = ForwardedProviderUsage {
            id: provider.id.clone(),
            name: provider.name.clone(),
            vendor: [provider.preset_key.as_deref(), provider.vendor.as_deref()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .find(|value| !value.is_empty())
                .map(str::to_ascii_lowercase),
            routes: routes.keys().cloned().collect(),
            status: ProviderUsageQueryStatus::Unsupported,
            error: None,
            usage: None,
            usage_entry: None,
        };
        let unknown = || {
            routes
                .iter()
                .map(|(route, models)| (route.clone(), vec![None; models.len()]))
                .collect()
        };
        // Chain guard: never call out to another Nyro from this endpoint.
        if crate::provider::nyro_gateway::is_nyro_gateway(&provider)
            || !provider_usage_monitorable(self, &provider).await
        {
            return (vec![entry], unknown());
        }
        let usage = match self
            .gw
            .usage_snapshots
            .fresh(&provider.id, FORWARDED_USAGE_MAX_AGE)
        {
            Some(usage) => Ok(usage),
            None => self.provider_usage_snapshot(&provider.id).await,
        };
        match usage {
            Ok(mut usage) => {
                // Cache age must not hide a newer owner/auth/shared denial from
                // older readers that only inspect each flat primary row.
                let current = self.gw.quota_registry.bucket_snapshot(&provider.id);
                for bucket in &mut usage.quota_buckets {
                    if let Some(latest) = current
                        .iter()
                        .find(|latest| latest.id == bucket.id && latest.scope_id == bucket.scope_id)
                        && latest.scheduling.status == ProviderSchedulingStatus::QuotaExhausted
                    {
                        bucket.scheduling = latest.scheduling.clone();
                    }
                }
                let owner = self.gw.quota_registry.snapshot(&provider.id);
                if matches!(
                    owner.reason.as_deref(),
                    Some(
                        "account_unavailable"
                            | "no_available_account"
                            | "auth_error"
                            | "authentication_failed"
                    )
                ) {
                    usage.is_available = Some(false);
                    usage.scheduling = owner;
                }
                let (rows, mut states) =
                    project_forwarded_entry(entry, &usage, &routes, include_money);
                // Include runtime auth/model cooldown fences that may have
                // arrived after this cached quota observation. Model identity
                // is the same concrete target used by route collection.
                for (route, models) in &routes {
                    if let Some(choices) = states.get_mut(route) {
                        for (choice, model) in choices.iter_mut().zip(models) {
                            if !self
                                .gw
                                .quota_registry
                                .is_model_schedulable(&provider.id, model)
                                && choice.as_ref().is_none_or(|state| {
                                    state.status != ProviderSchedulingStatus::QuotaExhausted
                                })
                            {
                                *choice = Some(ProviderScheduling {
                                    status: ProviderSchedulingStatus::QuotaExhausted,
                                    reason: Some("model_unavailable".into()),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
                (rows, states)
            }
            Err(error) => {
                tracing::warn!(%error, provider_id = %provider.id, "usage forwarding query failed");
                entry.status = ProviderUsageQueryStatus::Error;
                // Vendor errors can contain raw account/project identifiers or
                // request URLs. Keep details in the local admin/log boundary.
                entry.error = Some(truncate_error(
                    "Usage query failed; contact the gateway administrator",
                ));
                (vec![entry], unknown())
            }
        }
    }
}

fn forwarded_target_enabled(balance: &str, target: &ModelBackend) -> bool {
    let balance = balance.parse::<ModelBalance>().unwrap_or_default();
    target.is_fallback
        || !matches!(balance, ModelBalance::Weighted | ModelBalance::Usage)
        || target.weight > 0
}

/// Only configured alternatives for this exact visible route participate (OR).
/// An unresolved target remains unknown/eligible-for-probing; it is never backed
/// by an unrelated healthy sibling's quota evidence.
fn alternative_scheduling(choices: &[Option<ProviderScheduling>]) -> ProviderScheduling {
    if choices
        .iter()
        .flatten()
        .any(|state| state.status == ProviderSchedulingStatus::Eligible)
    {
        return ProviderScheduling::default();
    }
    if choices.is_empty() || choices.iter().any(Option::is_none) {
        return ProviderScheduling {
            reason: Some("unknown_mapping".into()),
            ..Default::default()
        };
    }
    let mut state = ProviderScheduling {
        status: ProviderSchedulingStatus::QuotaExhausted,
        reason: Some("remote_route_exhausted".into()),
        ..Default::default()
    };
    state.blocking_tiers = choices
        .iter()
        .flatten()
        .flat_map(|state| state.blocking_tiers.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // OR alternatives recover when any one recovers. Unknown reset stays open.
    state.reset_at = choices
        .iter()
        .flatten()
        .map(|state| {
            state
                .reset_at
                .as_deref()
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        })
        .collect::<Option<Vec<_>>>()
        .and_then(|resets| resets.into_iter().min())
        .map(|time| time.to_rfc3339());
    state
}

fn project_forwarded_entry(
    mut entry: ForwardedProviderUsage,
    usage: &ProviderUsage,
    routes: &BTreeMap<String, BTreeSet<String>>,
    include_money: bool,
) -> (
    Vec<ForwardedProviderUsage>,
    BTreeMap<String, Vec<Option<ProviderScheduling>>>,
) {
    let primary = usage
        .quota_buckets
        .iter()
        .filter(|bucket| bucket.is_primary())
        .collect::<Vec<_>>();
    if primary.is_empty() && usage.quota_buckets.is_empty() {
        entry.status = ProviderUsageQueryStatus::Ok;
        entry.usage = Some(PublicProviderUsage::from_usage(usage, include_money, false));
        let states = routes
            .iter()
            .map(|(route, models)| {
                (
                    route.clone(),
                    models
                        .iter()
                        .map(|_| Some(usage.scheduling.clone()))
                        .collect(),
                )
            })
            .collect();
        return (vec![entry], states);
    }
    let mut visible: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    let mut unresolved = BTreeSet::new();
    let mut states = BTreeMap::new();
    for (route, models) in routes {
        let mut choices = Vec::new();
        for model in models {
            let matching = primary
                .iter()
                .enumerate()
                .filter(|(_, bucket)| {
                    bucket.binds_model(model)
                        && bucket.source != "catalog"
                        && !bucket_tiers_conflict(&bucket.tiers)
                })
                .collect::<Vec<_>>();
            if let [(index, bucket)] = matching.as_slice() {
                visible.entry(*index).or_default().insert(route.clone());
                choices.push(Some(effective_bucket_scheduling(usage, bucket)));
            } else {
                unresolved.insert(route.clone());
                // Preserve explicit owner/auth or shared/global denial, but do
                // not borrow a sibling bucket as a model mapping.
                choices.push(
                    (usage.is_available == Some(false)).then(|| ProviderScheduling {
                        status: ProviderSchedulingStatus::QuotaExhausted,
                        reason: Some("account_unavailable".into()),
                        ..Default::default()
                    }),
                );
            }
        }
        states.insert(route.clone(), choices);
    }
    let mut rows = Vec::new();
    for (index, routes) in visible {
        let bucket = primary[index];
        let projected = bucket_usage(usage, bucket);
        let mut row = entry.clone();
        row.id = projected.provider_id.clone();
        row.name = format!("{} · {}", entry.name, bucket.label);
        row.routes = routes.into_iter().collect();
        row.status = ProviderUsageQueryStatus::Ok;
        row.usage = Some(PublicProviderUsage::from_usage(
            &projected,
            false,
            bucket.stale,
        ));
        row.usage_entry = Some(super::usage_publication::PublishedUsageEntry {
            kind: "quota_bucket",
            source_provider_ref: opaque_quota_id("forwarded_provider_v1", &[&entry.id]),
            bucket_ref: opaque_quota_id(
                "forwarded_bucket_v1",
                &[&entry.id, &bucket.scope_id, &bucket.id],
            ),
            binding_status: bucket.binding.status.clone(),
            stale: bucket.stale,
        });
        rows.push(row);
    }
    if !unresolved.is_empty() {
        // One generic diagnostic, no guessed children, hidden labels or model
        // membership. Not an ordinary healthy quota supplier for older peers.
        entry.id = opaque_quota_id("forwarded_unresolved_v1", &[&entry.id]);
        entry.routes = unresolved.into_iter().collect();
        entry.status = ProviderUsageQueryStatus::Error;
        entry.error = Some("Quota association unavailable for this route".into());
        entry.usage = None;
        rows.push(entry);
    }
    (rows, states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use crate::provider::nyro_gateway::usage::{
        forwarded_route_buckets, upstream_scheduling_observation,
    };
    use crate::router::quota::{
        ProviderQuotaRegistry, ProviderScheduling, ProviderSchedulingStatus,
    };

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
            api_price: None,
            account_pool: None,
            pool_quota: None,
            upstream: Some(serde_json::json!({"providers": []})),
            quota_buckets: Vec::new(),
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
        let wire = serde_json::to_value(usage).unwrap();
        assert!(wire.get("upstream").is_none(), "forwarding is one hop deep");
        assert!(wire.get("account_pool").is_none());

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
    async fn anonymous_pool_quota_survives_both_money_permissions_without_identity() {
        let f = fixture().await;
        let window = PoolQuotaWindow {
            total_accounts: 2,
            capacity_complete: true,
            total_units: 2,
            known_accounts: 1,
            known_remaining_units: 0.7,
            used_units: None,
            remaining_units: None,
            used_percent: None,
            remaining_percent: None,
            stale: true,
            next_reset_at: Some("2030-01-07T00:00:00Z".into()),
            next_reset_accounts: 1,
            reset_mode: "staggered".into(),
        };
        let quota = AccountPoolQuota {
            daily: None,
            weekly: None,
            claude_windows: vec![ClaudePoolQuotaWindow {
                name: "weekly_limit".into(),
                quota: window,
            }],
            estimated: true,
        };
        let view = AccountPoolView {
            provider_id: f.deepseek.clone(),
            kind: Some("claude_code".into()),
            config: Some(DevinPoolConfig::default()),
            accounts: vec![ProviderAccountView {
                account_id: "PRIVATE-ACCOUNT".into(),
                provider_id: f.deepseek.clone(),
                label: "PRIVATE-EMAIL@example.org".into(),
                is_enabled: true,
                priority: 0,
                status: "connected".into(),
                status_version: 1,
                verified_plan: None,
                identity_verified: false,
                scheduling: "included".into(),
                daily: None,
                weekly: None,
                reserve_usd: Some(123.0),
                kiro_usage: None,
                claude_usage: None,
                observed_at: None,
                stale: true,
                updated_at: String::new(),
            }],
            quota: quota.clone(),
            counts: AccountPoolCounts::default(),
            readiness: "eligible".into(),
        };
        let mut cached = snapshot(&f.deepseek, false);
        cached.kind = "claude_code_plan".into();
        cached.tiers.clear();
        cached.pool_quota = Some((&quota).into());
        cached.account_pool = Some(view);
        f.gw.usage_snapshots.store(&cached);
        let expected = serde_json::to_value(&quota).unwrap();
        for privileged in [false, true] {
            let report =
                f.gw.admin()
                    .forwarded_provider_usage(&RouteAccess {
                        privileged,
                        bound_route_ids: HashSet::from([f.bound_route.clone()]),
                    })
                    .await
                    .unwrap();
            let usage = report
                .providers
                .iter()
                .find(|p| p.id == f.deepseek)
                .unwrap()
                .usage
                .as_ref()
                .unwrap();
            assert!(
                usage.tiers.is_empty(),
                "estimates never become routing tiers"
            );
            assert_eq!(usage.scheduling.status, ProviderSchedulingStatus::Eligible);
            assert_eq!(usage.balances.is_empty(), !privileged);
            assert_eq!(usage.spends.is_empty(), !privileged);
            assert_eq!(report.redacted.is_empty(), privileged);
            assert_eq!(serde_json::to_value(&usage.pool_quota).unwrap(), expected);
            let wire = serde_json::to_value(usage).unwrap();
            assert!(wire.get("account_pool").is_none());
            assert!(wire.get("upstream").is_none());
            let text = wire.to_string();
            for private in [
                "PRIVATE-",
                "account_id",
                "label",
                "config",
                "reserve_usd",
                "\"accounts\"",
            ] {
                assert!(!text.contains(private), "leaked {private}");
            }
        }
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

    #[tokio::test]
    async fn fallback_and_secondary_targets_are_attributed_to_the_route() {
        // Regression: the SQL route snapshot only carries the primary
        // provider, so a provider that backs a route as a fallback (Devin
        // behind Kiro on a remote) used to vanish from the report.
        let dir = tempfile::tempdir().unwrap();
        let gw = gateway(&dir).await;
        let primary = provider(&gw, "Primary", None, "https://a.example/v1", true).await;
        let fallback = provider(&gw, "Fallback", None, "https://b.example/v1", true).await;
        let backend = |provider_id: &str, priority: i32, is_fallback: bool| CreateModelBackend {
            provider_id: provider_id.into(),
            model: "m".into(),
            weight: Some(100),
            priority: Some(priority),
            is_fallback: Some(is_fallback),
        };
        let route_id = gw
            .admin()
            .create_model(CreateModel {
                name: "opus".into(),
                balance: Some("priority".into()),
                target_provider: String::new(),
                target_model: String::new(),
                targets: vec![backend(&primary, 1, false), backend(&fallback, 2, true)],
                enable_auth: Some(true),
                enable_payload: Some(false),
                force_max_reasoning: None,
                vision_shim: None,
            })
            .await
            .expect("create route")
            .id;

        let access = RouteAccess {
            privileged: false,
            bound_route_ids: HashSet::from([route_id]),
        };
        let report = gw.admin().forwarded_provider_usage(&access).await.unwrap();
        assert_eq!(names(&report), ["Fallback", "Primary"]);
        for entry in &report.providers {
            assert_eq!(entry.routes, ["opus"], "{} serves the route", entry.name);
        }
    }

    fn admin_price() -> ProviderApiPriceStats {
        ProviderApiPriceStats {
            all_time: AccountUsageTotals {
                total_tokens: 123,
                request_count: 1,
                equivalent_costs: vec![EquivalentCostTotal {
                    currency: "USD".into(),
                    total_cost: 999.0,
                    priced_request_count: 1,
                }],
                unpriced_request_count: 0,
            },
            period: None,
            period_start: None,
            period_end: None,
            period_tier: None,
            used_percent: None,
            estimated_capacity: None,
            all_time_coverage: None,
            period_coverage: None,
            estimated_capacity_partial: None,
            unassigned: None,
            unassigned_coverage: None,
            retained_buckets: Vec::new(),
            models: Vec::new(),
        }
    }

    #[tokio::test]
    async fn admin_api_prices_are_never_cached_or_forwarded_even_for_privileged_keys() {
        let f = fixture().await;
        let mut usage = snapshot(&f.deepseek, false);
        usage.api_price = Some(admin_price());
        f.gw.usage_snapshots.store(&usage);
        assert!(
            f.gw.usage_snapshots
                .fresh(&f.deepseek, FORWARDED_USAGE_MAX_AGE)
                .unwrap()
                .api_price
                .is_none(),
            "the forwarding cache must not retain local admin prices"
        );
        // Bypass cache.store to exercise the redaction boundary independently,
        // including snapshots left over from an older cache implementation.
        f.gw.usage_snapshots
            .entries
            .write()
            .unwrap()
            .insert(f.deepseek.clone(), (Instant::now(), usage));
        for privileged in [false, true] {
            let report =
                f.gw.admin()
                    .forwarded_provider_usage(&RouteAccess {
                        privileged,
                        bound_route_ids: HashSet::from([f.bound_route.clone()]),
                    })
                    .await
                    .unwrap();
            let entry = report
                .providers
                .iter()
                .find(|entry| entry.id == f.deepseek)
                .unwrap();
            assert!(
                serde_json::to_value(entry).unwrap()["usage"]
                    .get("api_price")
                    .is_none()
            );
        }
    }

    fn bucket(id: &str, model: &str, paused: bool) -> crate::quota_buckets::QuotaBucket {
        crate::quota_buckets::QuotaBucket {
            id: id.into(),
            scope_id: "opaque-scope".into(),
            label: id.into(),
            models: vec![model.into()],
            binding: crate::quota_buckets::QuotaBucketBinding {
                status: "resolved".into(),
                source: "official".into(),
                revision: 1,
            },
            tiers: vec![ProviderUsageTier {
                name: "five_hour".into(),
                used_percent: if paused { 100.0 } else { 10.0 },
                resets_at: None,
            }],
            constraint_ids: Vec::new(),
            source: "enforced_summary".into(),
            queried_at: "2026-11-05T08:00:00Z".into(),
            stale: false,
            scheduling: ProviderScheduling {
                status: if paused {
                    ProviderSchedulingStatus::QuotaExhausted
                } else {
                    ProviderSchedulingStatus::Eligible
                },
                ..Default::default()
            },
            api_price: Some(admin_price()),
        }
    }

    #[tokio::test]
    async fn bucket_routes_use_exact_backend_models_and_do_not_expose_hidden_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let gw = gateway(&dir).await;
        let provider_id = provider(
            &gw,
            "Google",
            Some("deepseek"),
            "https://api.deepseek.com",
            true,
        )
        .await;
        let mut usage = snapshot(&provider_id, false);
        usage.quota_buckets = vec![
            bucket("Flash", "flash-wire", true),
            bucket("PRIVATE-Argon", "argon-wire", false),
        ];
        gw.usage_snapshots.store(&usage);
        let mut public_routes = Vec::new();
        for (name, model, enable_auth) in [
            ("flash-public", "flash-wire", false),
            ("flash-alias", "flash-wire", false),
            ("argon-secret", "argon-wire", true),
        ] {
            let id = gw
                .admin()
                .create_model(CreateModel {
                    name: name.into(),
                    balance: Some("priority".into()),
                    target_provider: provider_id.clone(),
                    target_model: model.into(),
                    targets: Vec::new(),
                    enable_auth: Some(enable_auth),
                    enable_payload: Some(false),
                    force_max_reasoning: None,
                    vision_shim: None,
                })
                .await
                .unwrap()
                .id;
            if !enable_auth {
                public_routes.push(id);
            }
        }
        let report = gw
            .admin()
            .forwarded_provider_usage(&RouteAccess::default())
            .await
            .unwrap();
        assert_eq!(
            report.providers.len(),
            1,
            "same bucket behind two routes emitted once"
        );
        let row = &report.providers[0];
        assert_eq!(row.name, "Google · Flash");
        assert_eq!(row.routes, ["flash-alias", "flash-public"]);
        assert_eq!(row.id, row.usage.as_ref().unwrap().provider_id);
        assert_eq!(
            row.usage.as_ref().unwrap().scheduling.status,
            ProviderSchedulingStatus::QuotaExhausted
        );
        assert!(
            report
                .route_scheduling
                .iter()
                .all(|route| route.scheduling.status == ProviderSchedulingStatus::QuotaExhausted)
        );
        let wire = serde_json::to_value(&report).unwrap();
        for forbidden in [
            "PRIVATE-Argon",
            "argon-wire",
            "flash-wire",
            "api_price",
            "scope_id",
            "quota_buckets",
            "account_pool",
        ] {
            assert!(!wire.to_string().contains(forbidden), "leaked {forbidden}");
        }
        // Display labels, reset/utilization changes, and a now-hidden sibling
        // never change the composite DSH key of the visible bucket.
        usage.quota_buckets[0].label = "Renamed Flash".into();
        usage.quota_buckets[0].tiers[0].used_percent = 99.0;
        usage.quota_buckets.pop();
        gw.usage_snapshots.store(&usage);
        let renamed = gw
            .admin()
            .forwarded_provider_usage(&RouteAccess::default())
            .await
            .unwrap();
        assert_eq!(row.id, renamed.providers[0].id);
        // Offline two-hop contract: downstream backend aliases are the remote
        // virtual routes, not Flash's hidden wire model or a healthy sibling.
        let ali_registry = ProviderQuotaRegistry::new();
        let remote_wire = serde_json::to_value(&report).unwrap();
        ali_registry.observe_buckets(
            "ali-relay",
            &forwarded_route_buckets("ali-relay", &remote_wire),
            None,
        );
        assert!(!ali_registry.is_model_schedulable("ali-relay", "flash-public"));
        assert!(!ali_registry.is_model_schedulable("ali-relay", "flash-alias"));
        assert!(ali_registry.is_model_schedulable("ali-relay", "unconfigured-other-route"));
        let legacy = upstream_scheduling_observation(&remote_wire);
        assert_eq!(
            legacy.len(),
            1,
            "every visible primary blocked keeps old reader safe"
        );
    }

    #[tokio::test]
    async fn forwarding_excludes_zero_weight_regular_rows_only_for_weighted_and_usage_routes() {
        let dir = tempfile::tempdir().unwrap();
        let gw = gateway(&dir).await;
        let id = provider(
            &gw,
            "Google",
            Some("deepseek"),
            "https://api.deepseek.com",
            true,
        )
        .await;
        let mut usage = snapshot(&id, false);
        usage.quota_buckets = vec![
            bucket("Flash", "flash-wire", true),
            bucket("Argon", "argon-wire", false),
        ];
        gw.usage_snapshots.store(&usage);
        for (name, balance, argon_weight, is_fallback) in [
            ("weighted-zero", "weighted", 0, false),
            ("usage-zero", "usage", 0, false),
            ("priority-zero", "priority", 0, false),
            ("latency-zero", "latency", 0, false),
            ("weighted-fallback-zero", "weighted", 0, true),
            ("usage-fallback-zero", "usage", 0, true),
        ] {
            let target = |model: &str, weight, priority, is_fallback| CreateModelBackend {
                provider_id: id.clone(),
                model: model.into(),
                weight: Some(weight),
                priority: Some(priority),
                is_fallback: Some(is_fallback),
            };
            gw.admin()
                .create_model(CreateModel {
                    name: name.into(),
                    balance: Some(balance.into()),
                    target_provider: String::new(),
                    target_model: String::new(),
                    targets: vec![
                        target("flash-wire", 100, 1, false),
                        target("argon-wire", argon_weight, 2, is_fallback),
                    ],
                    enable_auth: Some(false),
                    enable_payload: Some(false),
                    force_max_reasoning: None,
                    vision_shim: None,
                })
                .await
                .unwrap();
        }
        let report = gw
            .admin()
            .forwarded_provider_usage(&RouteAccess::default())
            .await
            .unwrap();
        let state = |route: &str| {
            report
                .route_scheduling
                .iter()
                .find(|row| row.route == route)
                .unwrap()
                .scheduling
                .status
        };
        for blocked in ["weighted-zero", "usage-zero"] {
            assert_eq!(
                state(blocked),
                ProviderSchedulingStatus::QuotaExhausted,
                "{blocked} must not borrow Argon's excluded row"
            );
        }
        for routable in [
            "priority-zero",
            "latency-zero",
            "weighted-fallback-zero",
            "usage-fallback-zero",
        ] {
            assert_eq!(
                state(routable),
                ProviderSchedulingStatus::Eligible,
                "{routable} retains the selector's zero-weight alternatives"
            );
        }
        let argon = report
            .providers
            .iter()
            .find(|row| row.name == "Google · Argon")
            .unwrap();
        assert_eq!(
            argon.routes,
            [
                "latency-zero",
                "priority-zero",
                "usage-fallback-zero",
                "weighted-fallback-zero"
            ]
        );
        let registry = ProviderQuotaRegistry::new();
        registry.observe_buckets(
            "relay",
            &forwarded_route_buckets("relay", &serde_json::to_value(&report).unwrap()),
            None,
        );
        assert!(!registry.is_model_schedulable("relay", "weighted-zero"));
        assert!(!registry.is_model_schedulable("relay", "usage-zero"));
        assert!(registry.is_model_schedulable("relay", "priority-zero"));
    }

    #[test]
    fn static_target_eligibility_matches_selector_for_cached_and_legacy_modes() {
        let target = |weight, is_fallback| ModelBackend {
            id: String::new(),
            model_id: "route".into(),
            provider_id: "p".into(),
            model: "m".into(),
            weight,
            priority: 1,
            is_fallback,
            created_at: String::new(),
        };
        for balance in ["weighted", "usage", " USAGE ", "invalid-defaults-weighted"] {
            assert!(!forwarded_target_enabled(balance, &target(0, false)));
            assert!(!forwarded_target_enabled(balance, &target(-1, false)));
            assert!(forwarded_target_enabled(balance, &target(100, false)));
            assert!(forwarded_target_enabled(balance, &target(0, true)));
        }
        for balance in ["priority", "latency"] {
            assert!(forwarded_target_enabled(balance, &target(0, false)));
            assert!(forwarded_target_enabled(balance, &target(-1, false)));
        }
    }

    #[test]
    fn unknown_binding_never_borrows_parent_routes_or_hidden_labels() {
        let mut usage = snapshot("parent", false);
        usage.quota_buckets = vec![bucket("PRIVATE-Hidden", "private-model", false)];
        let entry = ForwardedProviderUsage {
            id: "parent".into(),
            name: "Google".into(),
            vendor: None,
            routes: vec!["visible".into()],
            status: ProviderUsageQueryStatus::Ok,
            error: None,
            usage: None,
            usage_entry: None,
        };
        let (rows, states) = project_forwarded_entry(
            entry,
            &usage,
            &BTreeMap::from([("visible".into(), BTreeSet::from(["unknown-backend".into()]))]),
            false,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ProviderUsageQueryStatus::Error);
        assert!(rows[0].usage.is_none());
        assert!(states["visible"][0].is_none());
        assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
    }

    #[test]
    fn catalog_only_routes_publish_one_diagnostic_and_unknown_admission() {
        let mut usage = snapshot("p", false);
        usage.quota_buckets = vec![
            bucket("Flash low", "flash-low", false),
            bucket("Flash high", "flash-high", false),
        ];
        for bucket in &mut usage.quota_buckets {
            bucket.source = "catalog".into();
            bucket.binding.source = "catalog_exact_model_v1".into();
            bucket.scheduling.reason = Some("unknown_mapping".into());
        }
        let entry = ForwardedProviderUsage {
            id: "p".into(),
            name: "Google".into(),
            vendor: None,
            routes: vec!["flash".into()],
            status: ProviderUsageQueryStatus::Ok,
            error: None,
            usage: None,
            usage_entry: None,
        };
        let (rows, states) = project_forwarded_entry(
            entry,
            &usage,
            &BTreeMap::from([(
                "flash".into(),
                BTreeSet::from(["flash-low".into(), "flash-high".into()]),
            )]),
            false,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ProviderUsageQueryStatus::Error);
        assert!(rows[0].usage.is_none());
        assert_eq!(
            alternative_scheduling(&states["flash"]).reason.as_deref(),
            Some("unknown_mapping")
        );
        assert!(!serde_json::to_string(&rows).unwrap().contains("Flash low"));
    }

    #[test]
    fn route_alternatives_are_or_and_shared_constraints_are_not_suppliers() {
        let blocked = ProviderScheduling {
            status: ProviderSchedulingStatus::QuotaExhausted,
            ..Default::default()
        };
        assert_eq!(
            alternative_scheduling(&[Some(blocked.clone())]).status,
            ProviderSchedulingStatus::QuotaExhausted
        );
        assert_eq!(
            alternative_scheduling(&[Some(blocked.clone()), Some(ProviderScheduling::default())])
                .status,
            ProviderSchedulingStatus::Eligible
        );
        assert_eq!(
            alternative_scheduling(&[Some(blocked), None]).status,
            ProviderSchedulingStatus::Eligible
        );
        let mut usage = snapshot("p", false);
        usage.quota_buckets = vec![
            bucket("Flash", "flash", false),
            bucket("Argon", "argon", false),
        ];
        let mut shared = bucket("Global", "", true);
        shared.binding.status = "shared_constraint".into();
        shared.models.clear();
        for bucket in &mut usage.quota_buckets {
            bucket.constraint_ids.push(shared.id.clone());
        }
        usage.quota_buckets.push(shared);
        let entry = ForwardedProviderUsage {
            id: "p".into(),
            name: "Google".into(),
            vendor: None,
            routes: vec!["flash".into(), "argon".into()],
            status: ProviderUsageQueryStatus::Ok,
            error: None,
            usage: None,
            usage_entry: None,
        };
        let (rows, _) = project_forwarded_entry(
            entry,
            &usage,
            &BTreeMap::from([
                ("flash".into(), BTreeSet::from(["flash".into()])),
                ("argon".into(), BTreeSet::from(["argon".into()])),
            ]),
            true,
        );
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.usage.as_ref().unwrap().scheduling.status
                    == ProviderSchedulingStatus::QuotaExhausted)
        );
        assert!(
            rows.iter()
                .all(|row| row.usage.as_ref().unwrap().balances.is_empty())
        );
        assert!(!serde_json::to_string(&rows).unwrap().contains("api_price"));
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
