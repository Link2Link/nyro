import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertCircle, Loader2, Plus, Trash2 } from "lucide-react";
import { backend } from "@/lib/backend";
import { useLocale } from "@/lib/i18n";
import { tierLabel } from "@/lib/provider-usage";
import type { QuotaBucket, QuotaBucketConfig, ManualQuotaBucketBinding } from "@/lib/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";

export function ProviderQuotaBucketEditor({ providerId, buckets, open, onOpenChange }: {
  providerId: string; buckets: QuotaBucket[]; open: boolean; onOpenChange: (open: boolean) => void;
}) {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const config = useQuery<QuotaBucketConfig>({
    queryKey: ["provider-quota-buckets-config", providerId],
    queryFn: () => backend("get_provider_quota_buckets_config", { id: providerId }), enabled: open,
  });
  return <Dialog open={open} onOpenChange={onOpenChange}>
    <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-3xl" onClick={(event) => event.stopPropagation()}>
      <DialogHeader><DialogTitle>{zh ? "模型与配额桶绑定" : "Model-to-quota bindings"}</DialogTitle>
        <DialogDescription>{zh ? "显式选择同一权益作用域内的观察资源与精确模型；不按百分比或模型名称猜测关系。" : "Explicitly bind observed resources within one entitlement scope to exact models. Matching percentages or model names do not establish a binding."}</DialogDescription>
      </DialogHeader>
      {config.isLoading ? <p className="flex items-center gap-2 text-sm text-slate-500"><Loader2 className="h-4 w-4 animate-spin" />{zh ? "加载中…" : "Loading…"}</p>
        : config.error ? <div role="alert" className="space-y-2 text-sm text-red-600"><p>{String(config.error)}</p><Button variant="outline" onClick={() => void config.refetch()}>{zh ? "重试" : "Retry"}</Button></div>
          : config.data && <BindingForm key={config.data.revision} providerId={providerId} buckets={buckets} config={config.data} zh={zh} onSaved={() => onOpenChange(false)} />}
    </DialogContent>
  </Dialog>;
}

/** Plain form state only: validation, grouping and admission stay in the backend. */
export function QuotaBindingFields({ binding, resources, onChange, onRemove, isZh = false, existing = false }: {
  binding: ManualQuotaBucketBinding; resources: QuotaBucket[]; onChange: (binding: ManualQuotaBucketBinding) => void;
  onRemove: () => void; isZh?: boolean; existing?: boolean;
}) {
  const scopes = [...new Set([binding.scope_id, ...resources.map((bucket) => bucket.scope_id)].filter(Boolean))];
  const choices = resources.filter((bucket) => bucket.scope_id === binding.scope_id && bucket.binding.source !== "configured" && bucket.source !== "catalog");
  const missing = binding.constraint_ids.filter((id) => !choices.some((bucket) => bucket.id === id));
  return <fieldset className="space-y-3 rounded-xl border border-slate-200 p-3">
    <div className="grid gap-3 sm:grid-cols-2">
      <label className="space-y-1 text-xs text-slate-500">{isZh ? "稳定绑定 ID（不可更名）" : "Stable binding ID (immutable)"}
        <Input value={binding.id} required readOnly={existing} onChange={(event) => onChange({ ...binding, id: event.target.value })} /></label>
      <label className="space-y-1 text-xs text-slate-500">{isZh ? "显示标签（默认英文）" : "Display label (English default)"}
        <Input value={binding.label} required onChange={(event) => onChange({ ...binding, label: event.target.value })} /></label>
    </div>
    <label className="block space-y-1 text-xs text-slate-500">{isZh ? "权益作用域" : "Entitlement scope"}
      <select className="h-10 w-full rounded-md border border-slate-200 bg-white px-2 font-mono text-[11px]" value={binding.scope_id} required
        onChange={(event) => onChange({ ...binding, scope_id: event.target.value, constraint_ids: [] })}>
        {!scopes.length && <option value="">{isZh ? "暂无观察资源，请先刷新用量" : "No observed resources; refresh usage first"}</option>}
        {scopes.map((scope) => <option key={scope} value={scope}>{scope}</option>)}
      </select>
    </label>
    <label className="block space-y-1 text-xs text-slate-500">{isZh ? "精确上游模型 ID（每行一个）" : "Exact upstream model IDs (one per line)"}
      <textarea className="min-h-20 w-full rounded-md border border-slate-200 bg-white p-2 font-mono text-xs" value={binding.models.join("\n")} required spellCheck={false}
        onChange={(event) => onChange({ ...binding, models: event.target.value.split("\n") })} />
    </label>
    <div className="space-y-2"><p className="text-xs font-medium text-slate-500">{isZh ? "适用资源（包括所有额度窗口与共享约束）" : "Applicable resources (all windows and shared constraints)"}</p>
      {choices.map((bucket) => <label key={JSON.stringify([bucket.scope_id, bucket.id])} className="flex items-start gap-2 rounded-lg bg-slate-50 p-2 text-xs">
        <input type="checkbox" className="mt-0.5" checked={binding.constraint_ids.includes(bucket.id)} onChange={(event) => onChange({ ...binding,
          constraint_ids: event.target.checked ? [...binding.constraint_ids, bucket.id] : binding.constraint_ids.filter((id) => id !== bucket.id) })} />
        <span className="min-w-0"><strong className="font-medium text-slate-700">{bucket.label}</strong> · {bucket.tiers.map((tier) => tierLabel(tier.name, isZh)).join(" + ")}
          <span className="block break-all font-mono text-[10px] text-slate-400">{bucket.id}</span></span>
      </label>)}
      {missing.map((id) => <label key={id} className="flex items-start gap-2 text-xs text-amber-700"><input type="checkbox" checked onChange={() => onChange({ ...binding, constraint_ids: binding.constraint_ids.filter((value) => value !== id) })} />
        <span className="break-all">{isZh ? "资源未在当前快照中，保存将由后端验证" : "Resource absent from current snapshot; backend will validate on save"}: {id}</span></label>)}
      {!choices.length && <p className="text-xs text-amber-700">{isZh ? "此作用域暂无可绑定观察资源。" : "No observed resources available in this scope."}</p>}
    </div>
    <Button type="button" variant="ghost" size="sm" onClick={onRemove} className="text-red-600"><Trash2 className="h-3.5 w-3.5" />{isZh ? "移除绑定" : "Remove binding"}</Button>
  </fieldset>;
}

function BindingForm({ providerId, buckets, config, zh, onSaved }: {
  providerId: string; buckets: QuotaBucket[]; config: QuotaBucketConfig; zh: boolean; onSaved: () => void;
}) {
  const [bindings, setBindings] = useState(() => config.bindings.map((binding) => ({ ...binding, models: [...binding.models], constraint_ids: [...binding.constraint_ids] })));
  const client = useQueryClient();
  const save = useMutation({
    mutationFn: () => backend<QuotaBucketConfig>("set_provider_quota_buckets_config", { id: providerId,
      config: { revision: config.revision, bindings: bindings.map((binding) => ({ ...binding,
        models: binding.models.map((model) => model.trim()).filter(Boolean) })) } }),
    onSuccess: async (next) => {
      client.setQueryData(["provider-quota-buckets-config", providerId], next);
      await client.invalidateQueries({ queryKey: ["provider-usage", providerId] });
      onSaved();
    },
  });
  return <form className="space-y-4" onSubmit={(event) => { event.preventDefault(); save.mutate(); }}>
    <p className="flex gap-2 rounded-lg bg-amber-50 p-3 text-xs text-amber-800"><AlertCircle className="h-4 w-4 shrink-0" />
      {zh ? "绑定会影响新请求的准入与归属。仅合并已有证据证明共享的窗口；不要以相同百分比、标签或价格认定共享。旧记录不会按新绑定强行分摊。" : "Bindings affect admission and attribution of new attempts. Combine only windows proven to share an allowance, never by equal percentages, labels or prices. New bindings do not reassign ambiguous historical logs."}
    </p>
    {bindings.map((binding, index) => <QuotaBindingFields key={index} binding={binding} resources={buckets} isZh={zh}
      existing={config.bindings.some((item) => item.id === binding.id)}
      onChange={(next) => setBindings((current) => current.map((item, i) => i === index ? next : item))}
      onRemove={() => setBindings((current) => current.filter((_, i) => i !== index))} />)}
    {!bindings.length && <p className="text-xs text-slate-500">{zh ? "无显式绑定，使用后端发现与已验证规则。" : "No explicit bindings; using backend discovery and validated rules."}</p>}
    {save.error && <p role="alert" className="break-words text-xs text-red-600">{String(save.error)}</p>}
    <div className="flex flex-wrap justify-between gap-2">
      <Button type="button" variant="outline" onClick={() => setBindings((current) => [...current, {
        id: typeof crypto !== "undefined" && typeof crypto.randomUUID === "function" ? crypto.randomUUID() : `binding-${Date.now()}-${Math.random().toString(16).slice(2)}`,
        scope_id: buckets[0]?.scope_id ?? "", label: "", models: [], constraint_ids: [],
      }])}><Plus className="h-4 w-4" />{zh ? "添加绑定" : "Add binding"}</Button>
      <Button type="submit" disabled={save.isPending}>{save.isPending && <Loader2 className="h-4 w-4 animate-spin" />}{zh ? "保存绑定" : "Save bindings"}</Button>
    </div>
  </form>;
}
