//! Read-only Google usage projection, never routing bindings or historical identity.
//! Call only on a response clone, after raw-bucket pricing. Retained resource IDs
//! are the only valid log/detail selectors; synthetic family IDs are not selectors.

use std::collections::{BTreeMap, BTreeSet};

use super::{QuotaBucket, QuotaBucketBinding, opaque_quota_id};
use crate::admin::{AccountUsageTotals, ProviderApiPriceStats, ProviderUsageTier};
use crate::router::quota::{ProviderScheduling, ProviderSchedulingStatus};

pub const GOOGLE_FAMILY_DISPLAY_SOURCE: &str = "google_family_display_v1";

/// This usage-only whitelist does not rewrite callable models or pricing IDs.
pub fn google_display_family(model: &str) -> Option<&'static str> {
    ["gemini-3.8-flash", "gemini-4-argon"]
        .into_iter()
        .find(|base| {
            model.strip_prefix(*base).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "" | "-extra-low" | "-low" | "-medium" | "-high" | "-tiered"
                )
            })
        })
}

pub fn is_google_display_bucket(bucket: &QuotaBucket) -> bool {
    bucket.source == GOOGLE_FAMILY_DISPLAY_SOURCE
        && bucket.binding.source == GOOGLE_FAMILY_DISPLAY_SOURCE
}

/// Two known families per owner at most; all original resources remain sidecars.
/// Unknown/generic labels never establish membership or a healthy family. Already
/// projected responses are returned unchanged, not recursively grouped.
pub fn google_display_buckets(buckets: &[QuotaBucket]) -> Vec<QuotaBucket> {
    if buckets.iter().any(is_google_display_bucket) {
        return buckets.to_vec();
    }
    let mut groups: BTreeMap<(&str, &str), Vec<&QuotaBucket>> = BTreeMap::new();
    for bucket in buckets.iter().filter(|bucket| bucket.is_primary()) {
        let families: BTreeSet<_> = bucket
            .models
            .iter()
            .filter_map(|id| google_display_family(id))
            .collect();
        for family in families {
            groups
                .entry((&bucket.scope_id, family))
                .or_default()
                .push(bucket);
        }
    }
    let mut result = Vec::new();
    for ((scope, family), mut sources) in groups {
        sources.sort_by(|a, b| a.id.cmp(&b.id));
        let duplicate_conflict = sources
            .windows(2)
            .any(|pair| pair[0].id == pair[1].id && pair[0] != pair[1]);
        let mut pending: Vec<_> = sources.iter().map(|bucket| bucket.id.clone()).collect();
        pending.extend(
            buckets
                .iter()
                .filter(|bucket| bucket.scope_id == scope && bucket.is_global_constraint())
                .map(|bucket| bucket.id.clone()),
        );
        let mut refs = BTreeSet::new();
        let mut complete = true;
        while let Some(id) = pending.pop() {
            if refs.contains(&id) {
                continue;
            }
            let matching: Vec<_> = buckets
                .iter()
                .filter(|bucket| bucket.scope_id == scope && bucket.id == id)
                .collect();
            if matching.is_empty() {
                complete = false;
                continue;
            }
            refs.insert(id);
            for resource in matching {
                pending.extend(resource.constraint_ids.iter().cloned());
            }
        }
        let applicable: Vec<_> = buckets
            .iter()
            .filter(|bucket| bucket.scope_id == scope && refs.contains(&bucket.id))
            .collect();
        let stale = applicable.iter().any(|bucket| bucket.stale);
        let resolved = complete
            && !duplicate_conflict
            && !stale
            && sources
                .iter()
                .all(|bucket| bucket.is_resolved() && !bucket.tiers.is_empty())
            && applicable.iter().all(|bucket| {
                bucket.source == "enforced_summary"
                    && bucket.binding.status != "conflict"
                    && bucket.tiers.iter().all(|tier| {
                        tier.used_percent.is_finite() && (0.0..=100.0).contains(&tier.used_percent)
                    })
            });
        let tiers = display_tiers(&sources);
        let allow_period = resolved
            && sources.iter().all(|bucket| {
                bucket.tiers.len() == tiers.len()
                    && bucket.tiers.iter().all(|tier| tiers.contains(tier))
            })
            // Sidecar windows may be refreshed independently of their primary.
            // Shared/global observations affect admission, never this cycle's
            // denominator. A mismatched non-shared child invalidates the cycle.
            && applicable
                .iter()
                .filter(|bucket| bucket.binding.status != "shared_constraint")
                .all(|bucket| {
                    bucket.queried_at == sources[0].queried_at
                        && bucket.tiers.iter().all(|tier| tiers.contains(tier))
                });
        result.push(QuotaBucket {
            id: opaque_quota_id(GOOGLE_FAMILY_DISPLAY_SOURCE, &[scope, family]),
            scope_id: scope.to_owned(),
            label: family.to_owned(),
            models: sources
                .iter()
                .flat_map(|bucket| &bucket.models)
                .filter(|model| google_display_family(model) == Some(family))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            binding: QuotaBucketBinding {
                status: if resolved { "resolved" } else { "unresolved" }.to_owned(),
                source: GOOGLE_FAMILY_DISPLAY_SOURCE.to_owned(),
                revision: 1,
            },
            tiers,
            constraint_ids: refs.into_iter().collect(),
            source: GOOGLE_FAMILY_DISPLAY_SOURCE.to_owned(),
            queried_at: sources
                .iter()
                .map(|bucket| bucket.queried_at.as_str())
                .min()
                .unwrap_or_default()
                .to_owned(),
            stale,
            scheduling: display_scheduling(&applicable, resolved),
            api_price: (!duplicate_conflict)
                .then(|| display_price(&sources, family, allow_period))
                .flatten(),
        });
    }
    result.extend(buckets.iter().cloned().map(|mut bucket| {
        if bucket.is_primary() {
            bucket.binding.status = "window_constraint".to_owned();
        }
        bucket
    }));
    result
}

/// Max is only a conservative display projection; it is not a capacity sum or
/// evidence that independent resources share a cycle. Raw windows remain intact.
fn display_tiers(sources: &[&QuotaBucket]) -> Vec<ProviderUsageTier> {
    let mut windows: BTreeMap<&str, Vec<&ProviderUsageTier>> = BTreeMap::new();
    for tier in sources.iter().flat_map(|bucket| &bucket.tiers) {
        windows.entry(&tier.name).or_default().push(tier);
    }
    let mut tiers: Vec<_> = windows
        .into_iter()
        .filter_map(|(name, values)| {
            let used_percent = values
                .iter()
                .filter(|tier| tier.used_percent.is_finite())
                .map(|tier| tier.used_percent.clamp(0.0, 100.0))
                .reduce(f64::max)?;
            Some(ProviderUsageTier {
                name: name.to_owned(),
                used_percent,
                resets_at: values
                    .iter()
                    .all(|tier| tier.resets_at == values[0].resets_at)
                    .then(|| values[0].resets_at.clone())
                    .flatten(),
            })
        })
        .collect();
    tiers.sort_by_key(|tier| {
        (
            match tier.name.as_str() {
                "five_hour" => 0,
                "daily" => 1,
                "weekly_limit" => 2,
                "monthly" => 3,
                _ => 4,
            },
            tier.name.clone(),
        )
    });
    tiers
}

fn display_scheduling(resources: &[&QuotaBucket], resolved: bool) -> ProviderScheduling {
    let blocked: Vec<_> = resources
        .iter()
        .filter(|bucket| {
            bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted
                || bucket.tiers.iter().any(|tier| tier.used_percent >= 100.0)
        })
        .collect();
    let mut scheduling = ProviderScheduling {
        reason: (!resolved).then(|| "unknown_mapping".to_owned()),
        next_check_at: resources
            .iter()
            .filter_map(|bucket| bucket.scheduling.next_check_at.clone())
            .min(),
        ..Default::default()
    };
    if let Some(first) = blocked.first() {
        scheduling.status = ProviderSchedulingStatus::QuotaExhausted;
        scheduling.reason = first
            .scheduling
            .reason
            .clone()
            .or_else(|| Some("usage_limit".to_owned()));
        scheduling.blocking_tiers = blocked
            .iter()
            .flat_map(|bucket| {
                bucket.scheduling.blocking_tiers.iter().cloned().chain(
                    bucket
                        .tiers
                        .iter()
                        .filter(|tier| tier.used_percent >= 100.0)
                        .map(|tier| tier.name.clone()),
                )
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        scheduling.reset_at = blocked
            .iter()
            .map(|bucket| {
                bucket
                    .scheduling
                    .reset_at
                    .as_deref()
                    .and_then(|reset| chrono::DateTime::parse_from_rfc3339(reset).ok())
            })
            .collect::<Option<Vec<_>>>()
            .and_then(|resets| resets.into_iter().max())
            .map(|reset| reset.to_rfc3339());
    }
    scheduling
}

fn empty_totals() -> AccountUsageTotals {
    AccountUsageTotals {
        total_tokens: 0,
        request_count: 0,
        equivalent_costs: Vec::new(),
        unpriced_request_count: 0,
    }
}

fn add_totals(total: &mut AccountUsageTotals, value: &AccountUsageTotals) -> Option<()> {
    total.total_tokens = total
        .total_tokens
        .checked_add(value.total_tokens)
        .filter(|v| *v >= 0)?;
    total.request_count = total
        .request_count
        .checked_add(value.request_count)
        .filter(|v| *v >= 0)?;
    total.unpriced_request_count = total
        .unpriced_request_count
        .checked_add(value.unpriced_request_count)
        .filter(|v| *v >= 0)?;
    for cost in &value.equivalent_costs {
        if !cost.total_cost.is_finite() || cost.total_cost < 0.0 || cost.priced_request_count < 0 {
            return None;
        }
        if let Some(old) = total
            .equivalent_costs
            .iter_mut()
            .find(|old| old.currency == cost.currency)
        {
            old.total_cost += cost.total_cost;
            if !old.total_cost.is_finite() {
                return None;
            }
            old.priced_request_count = old
                .priced_request_count
                .checked_add(cost.priced_request_count)?;
        } else {
            total.equivalent_costs.push(cost.clone());
        }
    }
    total
        .equivalent_costs
        .sort_by(|a, b| a.currency.cmp(&b.currency));
    Some(())
}

// Coverage/model types are owned by admin pricing; infer them from cloned stats
// instead of exposing a second DTO or changing the storage/accounting contracts.
macro_rules! add_coverage {
    ($total:expr, $value:expr) => {
        for_field!(
            $total,
            $value,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            missing_usage_request_count,
            cache_read_reported_request_count,
            priced_request_count,
            unpriced_request_count,
            attributed_request_count,
            inferred_request_count,
            unassigned_request_count,
            legacy_timestamp_request_count
        );
    };
}
macro_rules! for_field {
    ($total:expr, $value:expr, $($field:ident),+) => { $(
        $total.$field = $total.$field.checked_add($value.$field).filter(|v| *v >= 0)?;
    )+ };
}

fn display_price(
    sources: &[&QuotaBucket],
    family: &str,
    allow_period: bool,
) -> Option<ProviderApiPriceStats> {
    let unique: BTreeMap<_, _> = sources
        .iter()
        .map(|bucket| (bucket.id.as_str(), *bucket))
        .collect();
    let mut result = unique.values().next()?.api_price.as_ref()?.clone();
    result.all_time = empty_totals();
    result.all_time_coverage = Some(Default::default());
    result.models.clear();
    let mut preserve_period = allow_period && unique.len() == 1;
    for source in unique.values() {
        let mut part = source.api_price.as_ref()?.clone();
        let pure = source
            .models
            .iter()
            .all(|id| google_display_family(id) == Some(family))
            && part
                .models
                .iter()
                .all(|row| google_display_family(&row.model) == Some(family));
        preserve_period &= pure;
        if !pure {
            // Current model membership cannot assign historical mixed attempts.
            if part.models.is_empty() && part.all_time.request_count > 0 {
                return None;
            }
            part.models
                .retain(|row| google_display_family(&row.model) == Some(family));
            part.all_time = empty_totals();
            let mut coverage = part.all_time_coverage.take().unwrap_or_default();
            let _ = std::mem::take(&mut coverage);
            for model in &part.models {
                add_totals(&mut part.all_time, &model.all_time)?;
                add_coverage!(coverage, model.coverage);
            }
            part.all_time_coverage = Some(coverage);
        }
        add_totals(&mut result.all_time, &part.all_time)?;
        if let (Some(total), Some(value)) = (&mut result.all_time_coverage, &part.all_time_coverage)
        {
            add_coverage!(total, value);
        } else {
            result.all_time_coverage = None;
        }
        for model in part.models {
            if let Some(old) = result
                .models
                .iter_mut()
                .find(|old| old.model == model.model)
            {
                add_totals(&mut old.all_time, &model.all_time)?;
                add_coverage!(old.coverage, model.coverage);
            } else {
                add_totals(&mut empty_totals(), &model.all_time)?;
                result.models.push(model);
            }
        }
    }
    result.models.sort_by(|a, b| a.model.cmp(&b.model));
    if !preserve_period {
        result.period = None;
        result.period_start = None;
        result.period_end = None;
        result.period_tier = None;
        result.used_percent = None;
        result.period_coverage = None;
        result.estimated_capacity = None;
        result.estimated_capacity_partial = None;
    } else {
        if let Some(period) = &result.period {
            add_totals(&mut empty_totals(), period)?;
        }
        if result
            .used_percent
            .is_none_or(|used| !used.is_finite() || used <= 0.0 || used > 100.0)
            || result.estimated_capacity.as_ref().is_some_and(|costs| {
                costs
                    .iter()
                    .any(|cost| !cost.total_cost.is_finite() || cost.total_cost < 0.0)
            })
        {
            result.estimated_capacity = None;
            result.estimated_capacity_partial = None;
        }
    }
    result.unassigned = None;
    result.unassigned_coverage = None;
    result.retained_buckets.clear();
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::EquivalentCostTotal;

    const FLASH: &str = "gemini-3.8-flash";
    const ARGON: &str = "gemini-4-argon";

    fn bucket(id: &str, model: &str, used: f64) -> QuotaBucket {
        QuotaBucket {
            id: id.to_owned(),
            scope_id: "owner-a".to_owned(),
            label: id.to_owned(),
            models: vec![model.to_owned()],
            binding: QuotaBucketBinding {
                status: "resolved".to_owned(),
                source: "official".to_owned(),
                revision: 2,
            },
            tiers: vec![ProviderUsageTier {
                name: "five_hour".to_owned(),
                used_percent: used,
                resets_at: Some("2030-01-01T05:00:00Z".to_owned()),
            }],
            constraint_ids: Vec::new(),
            source: "enforced_summary".to_owned(),
            queried_at: "2030-01-01T01:00:00Z".to_owned(),
            stale: false,
            scheduling: ProviderScheduling::default(),
            api_price: None,
        }
    }

    fn price(requests: i64, cost: f64) -> ProviderApiPriceStats {
        let totals = AccountUsageTotals {
            total_tokens: requests * 10,
            request_count: requests,
            equivalent_costs: vec![EquivalentCostTotal {
                currency: "USD".to_owned(),
                total_cost: cost,
                priced_request_count: requests,
            }],
            unpriced_request_count: 0,
        };
        ProviderApiPriceStats {
            all_time: totals.clone(),
            period: Some(totals),
            period_start: Some(1),
            period_end: Some(2),
            period_tier: Some("five_hour".to_owned()),
            used_percent: Some(20.0),
            estimated_capacity: Some(vec![EquivalentCostTotal {
                currency: "USD".to_owned(),
                total_cost: cost * 5.0,
                priced_request_count: requests,
            }]),
            all_time_coverage: Some(Default::default()),
            period_coverage: Some(Default::default()),
            estimated_capacity_partial: Some(false),
            unassigned: None,
            unassigned_coverage: None,
            retained_buckets: Vec::new(),
            models: Vec::new(),
        }
    }

    fn groups(buckets: &[QuotaBucket]) -> Vec<QuotaBucket> {
        google_display_buckets(buckets)
            .into_iter()
            .filter(is_google_display_bucket)
            .collect()
    }

    #[test]
    fn exact_two_families_accept_all_known_tiers_not_lookalikes() {
        for base in [FLASH, ARGON] {
            for suffix in ["", "-low", "-medium", "-high", "-extra-low", "-tiered"] {
                assert_eq!(
                    google_display_family(&format!("{base}{suffix}")),
                    Some(base)
                );
            }
            for suffix in [
                "-new",
                "-max",
                "-minimal",
                "-none",
                "-xhigh",
                "-high-extra",
                "-image-high",
                "-preview",
                "er",
                "-",
            ] {
                assert_eq!(google_display_family(&format!("{base}{suffix}")), None);
            }
        }
        for model in [
            "Gemini-4-argon",
            "gemini-4-argon-high ",
            "xgemini-4-argon",
            "gemini-4.1-argon",
            "gemini-3.8-flashlight",
            "claude-opus-5-5-high",
        ] {
            assert_eq!(google_display_family(model), None, "{model}");
        }
    }

    #[test]
    fn known_variants_fold_but_all_original_resources_survive() {
        let input: Vec<_> = [
            FLASH,
            "gemini-3.8-flash-low",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-tiered",
            ARGON,
            "gemini-4-argon-low",
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-other",
            "claude-other",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, model)| bucket(&i.to_string(), model, 10.0 + i as f64))
        .collect();
        let output = google_display_buckets(&input);
        assert_eq!(
            output.iter().filter(|bucket| bucket.is_primary()).count(),
            2
        );
        assert_eq!(output.len(), input.len() + 2);
        for original in &input {
            let retained = output
                .iter()
                .find(|bucket| bucket.id == original.id)
                .unwrap();
            let mut expected = original.clone();
            expected.binding.status = "window_constraint".to_owned();
            assert_eq!(*retained, expected);
        }
        let flash = output.iter().find(|bucket| bucket.label == FLASH).unwrap();
        assert_eq!(flash.models.len(), 5);
        assert_eq!(flash.tiers[0].used_percent, 14.0);
        assert_eq!(flash.constraint_ids.len(), 5);
    }

    #[test]
    fn identities_ignore_order_prices_usage_and_cutoff_but_not_owner() {
        let mut input = vec![
            bucket("low", "gemini-4-argon-low", 15.0),
            bucket("high", "gemini-4-argon-high", 20.0),
        ];
        let first = groups(&input).remove(0);
        input.reverse();
        input[0].queried_at = "2030-01-01T02:00:00Z".to_owned();
        input[0].tiers[0].used_percent = 90.0;
        input[0].api_price = Some(price(1, 99.0));
        assert_eq!(first.id, groups(&input)[0].id);
        let mut other = input[0].clone();
        other.scope_id = "owner-b".to_owned();
        input.push(other);
        let output = groups(&input);
        assert_eq!(output.len(), 2);
        assert_ne!(output[0].id, output[1].id);
        assert_eq!(output[1].models.len(), 1);
    }

    #[test]
    fn catalog_has_conservative_max_and_no_cycle_or_healthy_binding() {
        let mut input = vec![
            bucket("a", "gemini-3.8-flash-low", 20.0),
            bucket("b", "gemini-3.8-flash-high", 70.0),
        ];
        for source in &mut input {
            source.source = "catalog".to_owned();
            source.binding.source = "catalog_exact_model_v1".to_owned();
            source.tiers[0].name = "catalog".to_owned();
            source.api_price = Some(price(2, 1.0));
        }
        let output = groups(&input).remove(0);
        assert_eq!(output.binding.status, "unresolved");
        assert_eq!(output.scheduling.reason.as_deref(), Some("unknown_mapping"));
        assert_eq!(output.tiers.len(), 1);
        assert_eq!(output.tiers[0].name, "catalog");
        assert_eq!(output.tiers[0].used_percent, 70.0);
        let stats = output.api_price.unwrap();
        assert_eq!(stats.all_time.request_count, 4);
        assert!(stats.period.is_none());
        assert!(stats.estimated_capacity.is_none());
    }

    #[test]
    fn single_fresh_source_keeps_three_amounts_and_resource_filters() {
        let mut source = bucket("actual-raw-id", ARGON, 20.0);
        source.api_price = Some(price(3, 4.0));
        let output = groups(&[source.clone()]).remove(0);
        assert_eq!(output.api_price, source.api_price);
        assert_eq!(output.constraint_ids, vec![source.id]);
        assert_ne!(output.id, output.constraint_ids[0]);
    }

    #[test]
    fn same_partition_is_not_counted_twice_and_multicurrency_stays_separate() {
        let mut a = bucket("same", FLASH, 20.0);
        a.api_price = Some(price(2, 1.0));
        let mut b = bucket("other", "gemini-3.8-flash-low", 20.0);
        b.api_price = Some(price(3, 5.0));
        b.api_price.as_mut().unwrap().all_time.equivalent_costs[0].currency = "CNY".to_owned();
        let output = groups(&[a.clone(), a, b]).remove(0);
        let stats = output.api_price.unwrap();
        assert_eq!(stats.all_time.request_count, 5);
        assert_eq!(stats.all_time.equivalent_costs.len(), 2);
        assert_eq!(stats.all_time.equivalent_costs[0].currency, "CNY");
        assert!(stats.period.is_none());
    }

    #[test]
    fn inconsistent_resets_and_cutoffs_withhold_cycle_and_keep_raw_windows() {
        let mut a = bucket("a", FLASH, 20.0);
        a.api_price = Some(price(1, 1.0));
        let mut b = bucket("b", "gemini-3.8-flash-low", 70.0);
        b.api_price = Some(price(1, 2.0));
        b.tiers[0].resets_at = Some("2030-01-01T06:00:00Z".to_owned());
        b.queried_at = "2030-01-01T02:00:00Z".to_owned();
        let output = google_display_buckets(&[a, b]);
        let family = output
            .iter()
            .find(|bucket| is_google_display_bucket(bucket))
            .unwrap();
        assert_eq!(family.tiers[0].used_percent, 70.0);
        assert!(family.tiers[0].resets_at.is_none());
        assert!(
            family
                .api_price
                .as_ref()
                .unwrap()
                .estimated_capacity
                .is_none()
        );
        assert_eq!(
            output
                .iter()
                .filter(|bucket| !is_google_display_bucket(bucket))
                .count(),
            2
        );
    }

    #[test]
    fn scoped_constraints_block_without_borrowing_foreign_owner_constraints() {
        let mut source = bucket("source", FLASH, 20.0);
        source.constraint_ids.push("shared".to_owned());
        let mut shared = bucket("shared", "unrelated", 100.0);
        shared.binding.status = "shared_constraint".to_owned();
        shared.scheduling.status = ProviderSchedulingStatus::QuotaExhausted;
        let mut foreign = shared.clone();
        foreign.scope_id = "owner-b".to_owned();
        let unknown = groups(&[source.clone(), foreign]).remove(0);
        assert_eq!(
            unknown.scheduling.status,
            ProviderSchedulingStatus::Eligible
        );
        assert_eq!(unknown.binding.status, "unresolved");
        assert!(!unknown.constraint_ids.contains(&"shared".to_owned()));
        let blocked = groups(&[source, shared]).remove(0);
        assert_eq!(
            blocked.scheduling.status,
            ProviderSchedulingStatus::QuotaExhausted
        );
        assert!(blocked.constraint_ids.contains(&"shared".to_owned()));
    }

    #[test]
    fn partial_conflict_and_primary_block_cannot_become_healthy() {
        let mut source = bucket("a", FLASH, 20.0);
        source.stale = true;
        let stale = groups(&[source.clone()]).remove(0);
        assert!(stale.stale);
        assert_eq!(stale.binding.status, "unresolved");
        source.stale = false;
        source.binding.status = "conflict".to_owned();
        source.scheduling.status = ProviderSchedulingStatus::QuotaExhausted;
        let conflict = groups(&[source]).remove(0);
        assert_eq!(conflict.binding.status, "unresolved");
        assert_eq!(
            conflict.scheduling.status,
            ProviderSchedulingStatus::QuotaExhausted
        );
    }

    #[test]
    fn diagnostics_without_exact_membership_create_no_family_and_projection_is_idempotent() {
        let mut diagnostic = bucket("diagnostic", "Gemini", 50.0);
        diagnostic.models.clear();
        diagnostic.label = "Gemini Flash".to_owned();
        diagnostic.binding.status = "unresolved".to_owned();
        assert!(groups(&[diagnostic]).is_empty());
        let projected = google_display_buckets(&[bucket("actual", FLASH, 30.0)]);
        assert_eq!(google_display_buckets(&projected), projected);
    }

    #[test]
    fn mixed_family_without_model_price_breakdown_is_not_duplicated() {
        let mut source = bucket("shared-accounting", FLASH, 20.0);
        source.models.push(ARGON.to_owned());
        source.api_price = Some(price(3, 4.0));
        let output = groups(&[source]);
        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|bucket| bucket.api_price.is_none()));
    }

    #[test]
    fn observed_priced_zero_stays_zero_but_zero_ratio_has_no_capacity() {
        let mut source = bucket("free", ARGON, 0.0);
        source.api_price = Some(price(1, 0.0));
        source.api_price.as_mut().unwrap().used_percent = Some(0.0);
        let stats = groups(&[source]).remove(0).api_price.unwrap();
        assert_eq!(stats.all_time.equivalent_costs[0].total_cost, 0.0);
        assert_eq!(stats.all_time.equivalent_costs[0].priced_request_count, 1);
        assert!(stats.period.is_some());
        assert!(stats.estimated_capacity.is_none());
    }

    #[test]
    fn independently_refreshed_child_cannot_supply_a_mismatched_cycle() {
        let mut source = bucket("primary", ARGON, 20.0);
        source.api_price = Some(price(2, 3.0));
        source.constraint_ids.push("child".to_owned());
        let mut child = bucket("child", ARGON, 20.0);
        child.binding.status = "window_constraint".to_owned();
        assert!(
            groups(&[source.clone(), child.clone()])[0]
                .api_price
                .as_ref()
                .unwrap()
                .period
                .is_some()
        );
        for mismatch in ["usage", "reset", "cutoff", "extra_window"] {
            let mut other = child.clone();
            match mismatch {
                "usage" => other.tiers[0].used_percent = 100.0,
                "reset" => other.tiers[0].resets_at = Some("2030-01-01T06:00:00Z".to_owned()),
                "cutoff" => other.queried_at = "2030-01-01T02:00:00Z".to_owned(),
                _ => other.tiers[0].name = "weekly_limit".to_owned(),
            }
            let projected = groups(&[source.clone(), other]).remove(0);
            let stats = projected.api_price.unwrap();
            assert_eq!(stats.all_time.request_count, 2, "{mismatch}");
            assert!(stats.period.is_none(), "{mismatch}");
            assert!(stats.estimated_capacity.is_none(), "{mismatch}");
        }
        // This shared budget is admission-only, not the Argon price denominator.
        child.binding.status = "shared_constraint".to_owned();
        child.tiers[0].used_percent = 100.0;
        child.queried_at = "2030-01-01T02:00:00Z".to_owned();
        let projected = groups(&[source, child]).remove(0);
        assert_eq!(
            projected.scheduling.status,
            ProviderSchedulingStatus::QuotaExhausted
        );
        assert!(projected.api_price.unwrap().period.is_some());
    }

    #[test]
    fn overflow_or_missing_source_prices_never_become_partial_complete_totals() {
        let mut a = bucket("a", FLASH, 20.0);
        a.api_price = Some(price(1, f64::MAX));
        let mut b = bucket("b", "gemini-3.8-flash-low", 20.0);
        b.api_price = Some(price(1, f64::MAX));
        assert!(groups(&[a.clone(), b.clone()])[0].api_price.is_none());
        b.api_price = None;
        assert!(groups(&[a, b])[0].api_price.is_none());
    }
}
