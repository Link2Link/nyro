import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useLocale } from "@/lib/i18n";
import { newDevinPoolConfig, type DevinPoolConfig } from "@/lib/devin-account-pool";

/** Local form state only; no account eligibility or quota calculations. */
export function AccountPoolSettings({ config, pending, onSave, vendor = "devin" }: {
  vendor?: "devin" | "kiro";
  config: DevinPoolConfig | null;
  pending: boolean;
  onSave: (config: DevinPoolConfig) => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [draft, setDraft] = useState<Omit<DevinPoolConfig, "plan">>(() => {
    const defaults = config ?? newDevinPoolConfig();
    return {
      strategy: defaults.strategy, session_affinity: defaults.session_affinity,
      affinity_ttl_seconds: defaults.affinity_ttl_seconds, max_account_attempts: defaults.max_account_attempts,
      reserve_policy: defaults.reserve_policy,
    };
  });
  const valid = Number.isSafeInteger(draft.affinity_ttl_seconds) && draft.affinity_ttl_seconds > 0 && draft.affinity_ttl_seconds <= 86400
    && Number.isSafeInteger(draft.max_account_attempts) && draft.max_account_attempts > 0 && draft.max_account_attempts <= 8;
  return <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); if (valid) onSave(draft); }}>
    {!config && <p className="rounded-md bg-amber-50 p-2 text-xs text-amber-800">{isZh
      ? "保存默认设置即可启用账号池，随后可添加账号。未启用时保留原单账号使用和重登流程。"
      : "Save the default settings to enable the pool, then add accounts. Until enabled, the legacy single-account usage and sign-in flow are preserved."}</p>}
    <p className="text-xs text-slate-600">{vendor === "kiro"
      ? (isZh ? "Kiro 各账号独立记录套餐、资源额度与重置时间；网关负责凭证自动续期与账号调度。" : "Each Kiro account keeps its own plan, resource allowances and reset times. The gateway handles automatic credential refresh and account scheduling.")
      : (isZh ? "账号类型自动识别，Pro / Team Full Seat 与 Max 可混用。Max 无日额度，周容量为 Pro 的 10 倍。" : "Account types are detected automatically. Pro / Team Full Seat and Max can share a pool. Max has no daily quota and 10× Pro weekly capacity.")}</p>
    <div className="grid gap-3 sm:grid-cols-2">
      <label className="space-y-1 text-xs font-medium text-slate-600">
        <span>{isZh ? "调度策略" : "Scheduling strategy"}</span>
        <select className="w-full rounded-md border border-slate-300 bg-white px-3 py-2 text-sm" value={draft.strategy} disabled={pending}
          onChange={(event) => setDraft({ ...draft, strategy: event.target.value as DevinPoolConfig["strategy"] })}>
          <option value="round_robin">{isZh ? "轮询" : "Round robin"}</option>
          <option value="fill_first">{isZh ? "优先用满" : "Fill first"}</option>
        </select>
      </label>
      <label className="space-y-1 text-xs font-medium text-slate-600">
        <span>{isZh ? "会话粘性 TTL（秒）" : "Session affinity TTL (seconds)"}</span>
        <Input type="number" min={1} max={86400} step={1} value={draft.affinity_ttl_seconds} disabled={pending}
          onChange={(event) => setDraft({ ...draft, affinity_ttl_seconds: Number(event.target.value) })} />
      </label>
      <label className="space-y-1 text-xs font-medium text-slate-600">
        <span>{isZh ? "最大账号尝试数" : "Maximum account attempts"}</span>
        <Input type="number" min={1} max={8} step={1} value={draft.max_account_attempts} disabled={pending}
          onChange={(event) => setDraft({ ...draft, max_account_attempts: Number(event.target.value) })} />
      </label>
    </div>
    <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={draft.session_affinity} disabled={pending}
      onChange={(event) => setDraft({ ...draft, session_affinity: event.target.checked })} />{isZh ? "启用会话粘性" : "Enable session affinity"}</label>
    <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={draft.reserve_policy === "fallback"} disabled={pending}
      onChange={(event) => setDraft({ ...draft, reserve_policy: event.target.checked ? "fallback" : "disabled" })} />{vendor === "kiro"
        ? (isZh ? "允许付费超额兜底（需上游已开启超额计费）" : "Allow paid overage fallback (requires upstream opt-in)")
        : (isZh ? "允许付费余额兜底" : "Allow paid balance fallback")}</label>
    <p className="text-xs text-amber-700">{isZh
      ? "这不是账单硬限额：一次请求仍可能触发上游自动超额扣费。严格禁止支出还需要关闭上游 overage。"
      : "This is not a billing hard limit: a request may still trigger automatic upstream overage. Disable upstream overage as well to prevent extra spending."}</p>
    <Button type="submit" disabled={pending || !valid}>{isZh ? "保存账号池设置" : "Save pool settings"}</Button>
  </form>;
}

// Keep existing Devin callers stable while sharing the scheduling controls.
export const DevinPoolSettings = AccountPoolSettings;
