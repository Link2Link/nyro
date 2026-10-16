import { useId, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Info, Loader2, RotateCcw, Save } from "lucide-react";
import { backend } from "@/lib/backend";
import { useLocale } from "@/lib/i18n";
import { MAX_TOKEN_PRICE, parseTokenPriceDraft, tokenPriceDraft, type TokenPriceDraft } from "@/lib/token-price";
import type { TokenPriceConfig, TokenPriceRates } from "@/lib/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";

export interface TokenPriceTarget {
  provider_id: string;
  provider_name: string;
  model: string;
}

export function TokenPriceDialog({ target, onClose, onSaved }: {
  target: TokenPriceTarget;
  onClose: () => void;
  onSaved: (config: TokenPriceConfig, reset: boolean) => void;
}) {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const queryClient = useQueryClient();
  const queryKey = ["token-price", target.provider_id, target.model];
  const config = useQuery<TokenPriceConfig>({
    queryKey,
    queryFn: ({ signal }) => backend("get_token_price", { providerId: target.provider_id, model: target.model }, { signal }),
    staleTime: 0,
    gcTime: 0,
    retry: false,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
  });
  const mutation = useMutation({
    mutationFn: (price: TokenPriceRates | null) => backend<TokenPriceConfig>("set_token_price", {
      input: { provider_id: target.provider_id, model: target.model, price },
    }),
    onSuccess: async (saved, price) => {
      queryClient.setQueryData(queryKey, saved);
      // A pre-save request must not replace the newly recalculated amounts.
      await queryClient.cancelQueries({ queryKey: ["token-usage"] });
      await queryClient.invalidateQueries({ queryKey: ["token-usage"] });
      onSaved(saved, price === null);
      onClose();
    },
  });
  return <Dialog open onOpenChange={(open) => { if (!open && !mutation.isPending) onClose(); }}>
    <DialogContent className="max-h-[90dvh] overflow-y-auto sm:w-[560px]" showCloseButton={!mutation.isPending}>
      <DialogHeader>
        <DialogTitle>{zh ? "设置模型价格" : "Set model prices"}</DialogTitle>
        <DialogDescription className="break-all pr-3">{target.model} · {target.provider_name || target.provider_id}</DialogDescription>
      </DialogHeader>
      {config.isFetching ? <div role="status" className="flex items-center gap-2 py-12 text-sm text-slate-500"><Loader2 className="h-4 w-4 animate-spin" />{zh ? "正在读取价格设置…" : "Loading price settings…"}</div>
        : config.isError ? <div role="alert" className="mt-4 space-y-3 text-sm text-red-600"><p>{zh ? "无法读取价格设置，请重试。" : "Unable to load price settings. Please retry."} {config.error instanceof Error ? config.error.message : String(config.error)}</p><Button variant="outline" onClick={() => void config.refetch()}>{zh ? "重试" : "Retry"}</Button></div>
        : config.data && <TokenPriceForm config={config.data} pending={mutation.isPending} error={mutation.error} onSubmit={(price) => mutation.mutate(price)} onCancel={onClose} />}
    </DialogContent>
  </Dialog>;
}

function TokenPriceForm({ config, pending, error, onSubmit, onCancel }: {
  config: TokenPriceConfig;
  pending: boolean;
  error: Error | null;
  onSubmit: (price: TokenPriceRates | null) => void;
  onCancel: () => void;
}) {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const id = useId();
  const [draft, setDraft] = useState<TokenPriceDraft>(() => tokenPriceDraft(config));
  const [submitted, setSubmitted] = useState(false);
  const [confirmReset, setConfirmReset] = useState(false);
  const parsed = parseTokenPriceDraft(draft);
  const automatic = config.default_price;
  const inherited = config.inherited_price ?? null;
  const rate = (value: number, currency: string) => new Intl.NumberFormat(locale, { style: "currency", currency, maximumFractionDigits: 8 }).format(value);
  const fields = [
    { key: "input", label: zh ? "普通输入单价" : "Uncached input rate", optional: false },
    { key: "cache", label: zh ? "缓存读取单价" : "Cache read rate", optional: true },
    { key: "output", label: zh ? "输出单价" : "Output rate", optional: false },
  ] as const;
  const change = (key: keyof TokenPriceDraft, value: string) => {
    setDraft((current) => ({ ...current, [key]: value }));
    setConfirmReset(false);
  };
  return <form className="mt-4 space-y-4" noValidate onSubmit={(event) => {
    event.preventDefault();
    setSubmitted(true);
    if (parsed.price && !pending && !confirmReset) onSubmit(parsed.price);
  }}>
    <div className="rounded-lg border border-border bg-muted/40 p-3 text-xs">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="font-medium text-slate-800">{zh ? "当前生效价格" : "Effective reference"}</span>
        <span className="rounded-md bg-background px-2 py-1 text-slate-500">{config.override_price ? (zh ? "使用此模型的自定义价格" : "This model’s custom rates") : inherited ? (zh ? "使用前缀继承的自定义价格" : "Inherited custom rates") : automatic ? (zh ? "使用自动参考价格" : "Automatic reference rates") : (zh ? "暂无可用价格" : "No rates available")}</span>
      </div>
      {inherited && <div className="mt-3 rounded-md border border-border bg-background p-2.5">
        <div className="grid grid-cols-3 gap-2 tabular-nums">{[
          [zh ? "输入" : "Input", rate(inherited.input_rate, inherited.currency)],
          [zh ? "缓存" : "Cache", inherited.cache_read_rate === null ? (zh ? "跟随输入价" : "Input rate") : rate(inherited.cache_read_rate, inherited.currency)],
          [zh ? "输出" : "Output", rate(inherited.output_rate, inherited.currency)],
        ].map(([label, value]) => <div key={label}><p className="text-slate-500">{label}</p><p className="mt-1 break-words font-medium text-slate-800">{value}</p></div>)}</div>
        <p className="mt-2 break-all text-[11px] text-slate-500">{zh ? `来自同提供商更长前缀规则 · ${inherited.currency} / 1M tokens · 继承自 ${inherited.pricing_model}` : `From a longer same-provider prefix rule · ${inherited.currency} / 1M tokens · inherited from ${inherited.pricing_model}`}</p>
      </div>}
      {automatic ? <div className={inherited ? "mt-2 opacity-70" : "mt-3"}>
        {!inherited && <p className="mb-2 font-medium text-slate-700">{zh ? "自动参考价" : "Automatic reference"}</p>}
        <div className="grid grid-cols-3 gap-2 tabular-nums">{[
          [zh ? "输入" : "Input", rate(automatic.input_rate, automatic.currency)],
          [zh ? "缓存" : "Cache", automatic.cache_read_rate === null ? (zh ? "跟随输入价" : "Input rate") : rate(automatic.cache_read_rate, automatic.currency)],
          [zh ? "输出" : "Output", rate(automatic.output_rate, automatic.currency)],
        ].map(([label, value]) => <div key={label}><p className="text-slate-500">{label}</p><p className="mt-1 break-words font-medium text-slate-800">{value}</p></div>)}</div>
        <p className="mt-2 break-all text-[11px] text-slate-500">{automatic.currency} / 1M tokens · {automatic.pricing_provider} · {automatic.pricing_model}</p>
      </div> : !inherited && <p className="mt-2 leading-relaxed text-slate-500">{zh ? "未找到可靠的自动价格。你可以在下方手动补价；恢复默认后将重新显示未计价。" : "No reliable automatic price is available. Set your own rates below; restoring defaults will make this model unpriced again."}</p>}
    </div>

    <fieldset disabled={pending} className="space-y-3 disabled:opacity-60">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-sm font-medium text-slate-800">{zh ? "自定义单价" : "Custom rates"}</h3>
        <span className="text-xs text-slate-500">{zh ? "每百万 Token（1M）" : "Per million tokens (1M)"}</span>
      </div>
      <div className="space-y-1.5">
        <label id={`${id}-currency-label`} className="text-xs text-slate-600">{zh ? "计价币种" : "Pricing currency"}</label>
        <Select value={draft.currency} onValueChange={(value) => change("currency", value)} disabled={pending}>
          <SelectTrigger aria-labelledby={`${id}-currency-label`}><SelectValue /></SelectTrigger>
          <SelectContent><SelectItem value="USD">$ USD · {zh ? "美元" : "US dollar"}</SelectItem><SelectItem value="CNY">¥ CNY · {zh ? "人民币" : "Chinese yuan"}</SelectItem></SelectContent>
        </Select>
      </div>
      <div className="grid gap-3 sm:grid-cols-3">{fields.map((field) => <div key={field.key} className="space-y-1.5">
        <label htmlFor={`${id}-${field.key}`} className="text-xs text-slate-600">{field.label}{!field.optional && " *"}</label>
        <Input id={`${id}-${field.key}`} type="text" inputMode="decimal" value={draft[field.key]} aria-invalid={submitted && parsed.invalid === field.key} aria-describedby={submitted && parsed.invalid === field.key ? `${id}-validation` : field.optional ? `${id}-cache-help` : undefined} onChange={(event) => change(field.key, event.target.value)} placeholder={field.optional ? (zh ? "留空跟随输入" : "Use input rate") : "0.00"} />
      </div>)}</div>
      <p id={`${id}-cache-help`} className="text-[11px] leading-relaxed text-slate-500">{zh ? "缓存单价留空时按普通输入价计算；填写 0 表示免费。修改币种不会自动换算数值。" : "Leave cache blank to use the input rate; 0 means free. Changing currency does not convert the numbers."}</p>
      {submitted && parsed.invalid && <p id={`${id}-validation`} role="alert" className="text-xs text-red-600">{zh ? `请输入有效的非负单价（最大 ${MAX_TOKEN_PRICE.toLocaleString(locale)}）；输入与输出单价不能为空。` : `Enter a non-negative rate up to ${MAX_TOKEN_PRICE.toLocaleString(locale)}. Input and output rates are required.`}</p>}
    </fieldset>

    <div className="flex items-start gap-2 rounded-lg border border-border p-3 text-xs leading-relaxed text-slate-500"><Info className="mt-0.5 h-4 w-4 shrink-0" /><p>{zh ? "仅作用于此提供商下的这个模型及以它为前缀的变体（如 claude-haiku-5-5 覆盖 claude-haiku-5-5-medium，除非后者单独设置了价格）。保存后，历史及后续用量都会按新价格重新估值；不会修改 Token 日志，也不会更改上游实际收费。" : "Applies to this model and its prefix variants on this provider (e.g. claude-haiku-5-5 covers claude-haiku-5-5-medium unless the latter has its own rates). Saved rates revalue historical and future usage without changing token logs or upstream billing."}</p></div>
    {error && <p role="alert" className="break-words rounded-lg border border-red-200 bg-red-50 p-3 text-xs text-red-600">{zh ? "保存失败，输入已保留。" : "Could not save. Your inputs are preserved."} {error.message ?? String(error)}</p>}
    {confirmReset && <div role="alert" className="rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700">
      <p>{zh ? "确认移除此模型的自定义价格？保留的历史用量将改用前缀继承的自定义价格或自动参考价重新估值。" : "Remove this model’s custom rates? Retained usage will be revalued at inherited prefix rates or the automatic reference price."}</p>
      <div className="mt-3 flex flex-wrap gap-2"><Button type="button" size="sm" variant="outline" disabled={pending} onClick={() => onSubmit(null)}>{pending && <Loader2 className="h-3.5 w-3.5 animate-spin" />}{zh ? "确认恢复默认" : "Confirm restore defaults"}</Button><Button type="button" size="sm" variant="ghost" disabled={pending} onClick={() => setConfirmReset(false)}>{zh ? "返回编辑" : "Keep editing"}</Button></div>
    </div>}
    <div className="flex flex-wrap items-center justify-between gap-2 border-t border-border pt-4">
      <Button type="button" size="sm" variant="ghost" disabled={!config.override_price || pending} onClick={() => setConfirmReset(true)}><RotateCcw className="h-3.5 w-3.5" />{zh ? "恢复默认" : "Restore defaults"}</Button>
      <div className="flex gap-2"><Button type="button" size="sm" variant="outline" disabled={pending} onClick={onCancel}>{zh ? "取消" : "Cancel"}</Button><Button type="submit" size="sm" disabled={pending || confirmReset}>{pending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Save className="h-3.5 w-3.5" />}{zh ? "保存价格" : "Save prices"}</Button></div>
    </div>
  </form>;
}
