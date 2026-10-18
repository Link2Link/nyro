import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useLocale } from "@/lib/i18n";
import { type DevinPlan, type DevinPoolConfig } from "@/lib/devin-account-pool";

export function DevinPlanSelect({ value, onChange, disabled = false }: {
  value: DevinPlan | "";
  onChange: (plan: DevinPlan) => void;
  disabled?: boolean;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  return <label className="block space-y-1 text-xs font-medium text-slate-600">
    <span>{isZh ? "套餐（必选）" : "Plan (required)"}</span>
    <select className="w-full rounded-md border border-slate-300 bg-white px-3 py-2 text-sm" value={value} disabled={disabled}
      onChange={(event) => { if (event.target.value === "pro" || event.target.value === "max") onChange(event.target.value); }}>
      <option value="" disabled>{isZh ? "请选择套餐" : "Select a plan"}</option>
      <option value="pro">Pro</option>
      <option value="max">Max</option>
    </select>
  </label>;
}

/** Local form state only; no account eligibility or quota calculations. */
export function DevinPoolSettings({ config, hasAccounts, pending, onSave }: {
  config: DevinPoolConfig | null;
  hasAccounts: boolean;
  pending: boolean;
  onSave: (config: DevinPoolConfig) => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [plan, setPlan] = useState<DevinPlan | "">(config?.plan ?? "");
  const [draft, setDraft] = useState<Omit<DevinPoolConfig, "plan">>(config ?? {
    strategy: "round_robin", session_affinity: true, affinity_ttl_seconds: 3600,
    max_account_attempts: 3, reserve_policy: "disabled",
  });
  const valid = plan !== "" && Number.isSafeInteger(draft.affinity_ttl_seconds) && draft.affinity_ttl_seconds > 0 && draft.affinity_ttl_seconds <= 86400
    && Number.isSafeInteger(draft.max_account_attempts) && draft.max_account_attempts > 0 && draft.max_account_attempts <= 8;
  return <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); if (valid && plan) onSave({ ...draft, plan }); }}>
    {!config && <p className="rounded-md bg-amber-50 p-2 text-xs text-amber-800">{isZh
      ? "请选择并验证套餐后再添加账号。此处未配置时保留原单账号路径，不推测套餐或池容量。"
      : "Select and validate a plan before adding accounts. Until configured, the existing single-account path is preserved without assuming a plan or pool capacity."}</p>}
    <div className="grid gap-3 sm:grid-cols-2">
      <DevinPlanSelect value={plan} onChange={setPlan} disabled={pending || (Boolean(config) && hasAccounts)} />
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
      onChange={(event) => setDraft({ ...draft, reserve_policy: event.target.checked ? "fallback" : "disabled" })} />{isZh ? "允许付费余额兜底" : "Allow paid balance fallback"}</label>
    <p className="text-xs text-amber-700">{isZh
      ? "这不是账单硬限额：一次请求仍可能触发上游自动超额扣费。严格禁止支出还需要关闭上游 overage。"
      : "This is not a billing hard limit: a request may still trigger automatic upstream overage. Disable upstream overage as well to prevent extra spending."}</p>
    <Button type="submit" disabled={pending || !valid}>{isZh ? "保存账号池设置" : "Save pool settings"}</Button>
  </form>;
}
