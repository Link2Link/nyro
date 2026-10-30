import { deepEqual, doesNotMatch, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { LocaleProvider, useLocale, type Locale } from "../lib/i18n";
import { newDevinPoolConfig, type DevinPoolConfig } from "../lib/devin-account-pool";
import { AccountPoolSettings } from "./devin-pool-settings";

type Vendor = "devin" | "kiro" | "claude_code";

function settings(vendor?: Vendor, config: DevinPoolConfig | null = null, locale: Locale = "en-US", pending = false) {
  function LocalizedSettings() {
    // SSR skips browser locale effects. Override only this test render's fresh context.
    // eslint-disable-next-line react-hooks/immutability -- test-only locale fixture; no mounted UI or shared context
    useLocale().locale = locale;
    return <AccountPoolSettings vendor={vendor} config={config} pending={pending} onSave={() => {}} />;
  }
  return renderToStaticMarkup(<LocaleProvider><LocalizedSettings /></LocaleProvider>);
}

function options(html: string) {
  return [...html.matchAll(/<option value="([^"]+)"/g)].map((option) => option[1]);
}

test("weekly-reset scheduling is offered only for Claude, with existing strategies unchanged", () => {
  for (const vendor of [undefined, "devin", "kiro"] as const) {
    const html = settings(vendor);
    deepEqual(options(html), ["round_robin", "fill_first"]);
    doesNotMatch(html, /weekly_reset_first|weekly quota reset|周重置/);
  }
  const html = settings("claude_code");
  deepEqual(options(html), ["round_robin", "fill_first", "weekly_reset_first"]);
  match(html, /<option value="weekly_reset_first">Soonest weekly reset first<\/option>/);
});

test("pool defaults and saved round-robin/fill-first selections remain unchanged", () => {
  deepEqual(newDevinPoolConfig(), {
    strategy: "round_robin", session_affinity: true, affinity_ttl_seconds: 3600,
    max_account_attempts: 3, reserve_policy: "disabled",
  });
  for (const vendor of [undefined, "devin", "kiro", "claude_code"] as const) {
    match(settings(vendor), /<option value="round_robin" selected="">Round robin<\/option>/);
    for (const strategy of ["round_robin", "fill_first"] as const) {
      const html = settings(vendor, { ...newDevinPoolConfig(), strategy });
      match(html, new RegExp(`<option value="${strategy}" selected="">`));
      doesNotMatch(html, /the soonest valid future weekly quota reset wins/);
    }
  }
});

test("saved weekly-reset strategy explains priority, future resets, unknown times and affinity in both locales", () => {
  const config: DevinPoolConfig = { ...newDevinPoolConfig(), strategy: "weekly_reset_first" };
  const en = settings("claude_code", config);
  match(en, /<option value="weekly_reset_first" selected="">Soonest weekly reset first<\/option>/);
  match(en, /Availability, quota tier and manual priority still take precedence/);
  match(en, /Within the same group, the soonest valid future weekly quota reset wins, not the latest usage fetch/);
  match(en, /Unknown, invalid or expired reset times sort last in that group/);
  match(en, /Enabled session affinity is preserved/);
  match(en, /type="checkbox" checked=""/);

  const zh = settings("claude_code", config, "zh-CN");
  match(zh, /<option value="weekly_reset_first" selected="">最近周重置优先<\/option>/);
  match(zh, /可用性、额度等级和手动优先级仍优先/);
  match(zh, /同组账号按下一次有效周额度重置时间由近到远排序，不看用量获取时间/);
  match(zh, /未知、无效或已过期的重置时间排在同组后/);
  match(zh, /启用的会话粘性仍保留/);
});

test("weekly-reset strategy retains disabled affinity and pending control behavior", () => {
  const html = settings("claude_code", { ...newDevinPoolConfig(), strategy: "weekly_reset_first", session_affinity: false }, "en-US", true);
  match(html, /<select[^>]*disabled=""/);
  match(html, /<option value="weekly_reset_first" selected="">/);
  match(html, /<input type="checkbox" disabled=""/);
  doesNotMatch(html, /<input type="checkbox"[^>]*checked=""/);
  match(html, /<button[^>]*disabled=""[^>]*>Save pool settings<\/button>/);
});
