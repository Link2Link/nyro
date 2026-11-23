import { useMemo, useState, type FormEvent } from "react";
import { CloudDownload, Link2, Loader2, Settings2, Trash2, TriangleAlert } from "lucide-react";
import { useLocale } from "@/lib/i18n";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { formatLocalDateTime } from "@/lib/format";
import { useAaRatingSync, useAaRatingSyncMutations } from "@/lib/use-model-ratings";
import type { AaModelRatingEntry } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";

export function AaRatingSyncBar() {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const aaSync = useAaRatingSync();
  const { triggerSync } = useAaRatingSyncMutations();
  const [configOpen, setConfigOpen] = useState(false);

  const overview = aaSync.data;
  const cfg = overview?.config;
  const status = overview?.status;
  const versionChanged = Boolean(
    status?.index_version
    && status?.previous_index_version
    && status.index_version !== status.previous_index_version,
  );

  return (
    <div className="glass rounded-2xl p-3.5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm font-medium text-slate-800">
              {isZh ? "Artificial Analysis 自动同步" : "Artificial Analysis Auto-Sync"}
            </span>
            {cfg?.enabled ? (
              <Badge variant="secondary" className="bg-emerald-50 text-emerald-700">
                {isZh ? `每 ${cfg.sync_interval_hours} 小时` : `Every ${cfg.sync_interval_hours}h`}
              </Badge>
            ) : (
              <Badge variant="outline" className="text-slate-500">
                {isZh ? "未启用自动同步" : "Auto-sync off"}
              </Badge>
            )}
            {status?.index_version && (
              <Badge variant="outline" className="border-sky-200 bg-sky-50 text-sky-700">
                AA Index v{status.index_version}
              </Badge>
            )}
            {overview && overview.models.length > 0 && (
              <span className="text-xs text-slate-500">
                {isZh
                  ? `已同步 ${overview.models.length} 个基础模型（最高推理档位）`
                  : `${overview.models.length} base models synced (highest reasoning tier)`}
              </span>
            )}
          </div>
          <p className="text-xs text-slate-500">
            {status?.last_success_at
              ? (isZh
                ? `上次成功同步：${formatLocalDateTime(status.last_success_at)} · 手工前缀评分优先于 AA 分数`
                : `Last synced: ${formatLocalDateTime(status.last_success_at)} · Manual prefix ratings override AA scores`)
              : (isZh
                ? "配置 AA API Key 后将按周期自动同步各模型最高推理档位的 Intelligence Index 分数。"
                : "Configure an Artificial Analysis API key to sync each model's highest reasoning tier Intelligence Index score automatically.")}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button
            variant="secondary"
            size="sm"
            disabled={triggerSync.isPending || !cfg?.has_api_key}
            onClick={() => triggerSync.mutate()}
          >
            {triggerSync.isPending
              ? <Loader2 className="h-3.5 w-3.5 animate-spin" />
              : <CloudDownload className="h-3.5 w-3.5" />}
            {isZh ? "立即同步" : "Sync now"}
          </Button>
          <Button variant="secondary" size="sm" onClick={() => setConfigOpen(true)}>
            <Settings2 className="h-3.5 w-3.5" />
            {isZh ? "同步设置" : "Sync settings"}
          </Button>
        </div>
      </div>

      {versionChanged && status?.index_version && status?.previous_index_version && (
        <div role="status" className="mt-2.5 flex items-start gap-2 rounded-xl border border-amber-200 bg-amber-50/70 px-3 py-2 text-xs text-amber-800">
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>
            {isZh
              ? `AA Intelligence Index 版本已从 v${status.previous_index_version} 升级至 v${status.index_version}，建议复核手工覆盖评分以保持刻度一致。`
              : `AA Intelligence Index upgraded from v${status.previous_index_version} to v${status.index_version}. Consider reviewing manual overrides for scale consistency.`}
          </span>
        </div>
      )}

      {(triggerSync.error || status?.last_error) && (
        <div role="alert" className="mt-2.5 flex items-start gap-2 rounded-xl border border-red-200 bg-red-50/70 px-3 py-2 text-xs text-red-700">
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span className="break-words">
            {isZh ? "AA 同步失败（已保留上次缓存）：" : "AA sync error (previous cache retained): "}
            {triggerSync.error
              ? localizeBackendErrorMessage(triggerSync.error, isZh)
              : status?.last_error}
          </span>
        </div>
      )}

      {configOpen && (
        <AaSyncSettingsDialog onClose={() => setConfigOpen(false)} />
      )}
    </div>
  );
}

function AaSyncSettingsDialog({ onClose }: { onClose: () => void }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const aaSync = useAaRatingSync();
  const { updateConfig } = useAaRatingSyncMutations();
  const cfg = aaSync.data?.config;

  const [enabled, setEnabled] = useState(cfg?.enabled ?? false);
  const [apiKeyDraft, setApiKeyDraft] = useState("");
  const [clearStoredKey, setClearStoredKey] = useState(false);
  const [intervalDraft, setIntervalDraft] = useState(String(cfg?.sync_interval_hours ?? 24));
  const [baseUrlDraft, setBaseUrlDraft] = useState(cfg?.api_base_url ?? "");
  const [validationError, setValidationError] = useState<string | null>(null);

  function handleSubmit(event: FormEvent) {
    event.preventDefault();
    const hours = Number(intervalDraft.trim());
    if (!Number.isInteger(hours) || hours < 1 || hours > 168) {
      setValidationError(isZh ? "同步周期须为 1–168 之间的整数小时。" : "Sync interval must be an integer from 1 to 168 hours.");
      return;
    }
    setValidationError(null);
    const payload: {
      enabled: boolean;
      sync_interval_hours: number;
      api_key?: string;
      api_base_url?: string;
    } = {
      enabled,
      sync_interval_hours: hours,
      api_base_url: baseUrlDraft.trim(),
    };
    if (clearStoredKey) {
      payload.api_key = "";
    } else if (apiKeyDraft.trim()) {
      payload.api_key = apiKeyDraft.trim();
    }
    updateConfig.mutate(payload, { onSuccess: onClose });
  }

  return (
    <Dialog open onOpenChange={(open) => { if (!open && !updateConfig.isPending) onClose(); }}>
      <DialogContent showCloseButton={!updateConfig.isPending}>
        <DialogHeader>
          <DialogTitle>{isZh ? "Artificial Analysis 同步设置" : "Artificial Analysis Sync Settings"}</DialogTitle>
          <DialogDescription>
            {isZh
              ? "每日自动拉取 AA /api/v2/language/models/free，按每个基础模型的最高推理档位生成固定评分。手工前缀评分始终优先。"
              : "Automatically fetches AA /api/v2/language/models/free and assigns each base model the score of its highest reasoning effort tier. Manual prefix ratings always take precedence."}
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="flex items-center justify-between gap-3 rounded-xl bg-slate-50 px-3.5 py-2.5">
            <div>
              <p className="text-sm font-medium text-slate-800">{isZh ? "启用每日自动同步" : "Enable automatic sync"}</p>
              <p className="text-xs text-slate-500">{isZh ? "后台定期检查并按设定周期同步。" : "Background worker checks periodically and syncs on schedule."}</p>
            </div>
            <Switch checked={enabled} onCheckedChange={setEnabled} />
          </div>

          <div className="space-y-1.5">
            <label className="text-xs font-medium text-slate-700">
              {isZh ? "AA API Key（x-api-key）" : "AA API Key (x-api-key)"}
            </label>
            <Input
              type="password"
              autoComplete="off"
              value={apiKeyDraft}
              disabled={updateConfig.isPending || clearStoredKey}
              onChange={(event) => setApiKeyDraft(event.target.value)}
              placeholder={cfg?.api_key_preview
                ? (isZh ? `已保存：${cfg.api_key_preview}（留空保持不变）` : `Saved: ${cfg.api_key_preview} (leave blank to keep)`)
                : (cfg?.env_api_key_set
                  ? (isZh ? "已通过 NYRO_AA_API_KEY 环境变量提供" : "Provided via NYRO_AA_API_KEY env var")
                  : (isZh ? "输入 Artificial Analysis 免费 API Key" : "Enter Artificial Analysis API key"))}
            />
            {cfg?.api_key_preview && (
              <label className="flex items-center gap-2 text-xs text-slate-500">
                <input
                  type="checkbox"
                  checked={clearStoredKey}
                  onChange={(event) => setClearStoredKey(event.target.checked)}
                />
                {isZh ? "清除已保存的 API Key" : "Clear stored API key"}
              </label>
            )}
          </div>

          <div className="space-y-1.5">
            <label className="text-xs font-medium text-slate-700">
              {isZh ? "同步周期（小时，默认 24）" : "Sync interval (hours, default 24)"}
            </label>
            <Input
              type="text"
              inputMode="numeric"
              value={intervalDraft}
              disabled={updateConfig.isPending}
              onChange={(event) => { setIntervalDraft(event.target.value); setValidationError(null); }}
              placeholder="24"
            />
          </div>

          <div className="space-y-1.5">
            <label className="text-xs font-medium text-slate-700">
              {isZh ? "自定义 API 地址（可选，留空用官方接口）" : "Custom API URL (optional, blank uses official endpoint)"}
            </label>
            <Input
              type="url"
              value={baseUrlDraft}
              disabled={updateConfig.isPending}
              onChange={(event) => setBaseUrlDraft(event.target.value)}
              placeholder="https://artificialanalysis.ai/api/v2/language/models/free"
            />
          </div>

          {validationError && <p role="alert" className="text-xs text-red-600">{validationError}</p>}
          {updateConfig.error && (
            <p role="alert" className="break-words text-xs text-red-600">
              {localizeBackendErrorMessage(updateConfig.error, isZh)}
            </p>
          )}

          <DialogFooter>
            <Button type="button" variant="secondary" disabled={updateConfig.isPending} onClick={onClose}>
              {isZh ? "取消" : "Cancel"}
            </Button>
            <Button type="submit" disabled={updateConfig.isPending}>
              {updateConfig.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
              {isZh ? "保存" : "Save"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function AaAliasBindDialog({
  modelPrefix,
  models,
  currentTargetKey,
  onClose,
}: {
  modelPrefix: string;
  models: AaModelRatingEntry[];
  currentTargetKey?: string | null;
  onClose: () => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const { setAlias, deleteAlias } = useAaRatingSyncMutations();
  const [prefixDraft, setPrefixDraft] = useState(modelPrefix);
  const [query, setQuery] = useState("");
  const [selectedKey, setSelectedKey] = useState(currentTargetKey ?? "");
  const pending = setAlias.isPending || deleteAlias.isPending;

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    const list = q
      ? models.filter((m) =>
        m.canonical_key.includes(q)
        || m.aa_name.toLowerCase().includes(q)
        || m.aa_creator.toLowerCase().includes(q))
      : models;
    return [...list].sort((a, b) => b.raw_score - a.raw_score).slice(0, 60);
  }, [models, query]);

  function handleSave(event: FormEvent) {
    event.preventDefault();
    if (!prefixDraft.trim() || !selectedKey) return;
    setAlias.mutate(
      { modelPrefix: prefixDraft.trim(), aaCanonicalKey: selectedKey },
      { onSuccess: onClose },
    );
  }

  return (
    <Dialog open onOpenChange={(open) => { if (!open && !pending) onClose(); }}>
      <DialogContent showCloseButton={!pending} className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Link2 className="h-4 w-4 text-sky-600" />
            {isZh ? "关联到 AA 模型" : "Bind prefix to AA model"}
          </DialogTitle>
          <DialogDescription>
            {isZh
              ? "将自定义模型前缀绑定到已同步的 Artificial Analysis 模型，之后分数会自动跟随 AA 每日同步更新。"
              : "Bind a custom model prefix to a synced Artificial Analysis model so its score updates automatically with daily AA syncs."}
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSave} className="space-y-3">
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-slate-700">{isZh ? "本地模型前缀" : "Local model prefix"}</label>
            <Input
              value={prefixDraft}
              onChange={(event) => setPrefixDraft(event.target.value)}
              disabled={pending}
              className="font-mono"
            />
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-slate-700">{isZh ? "搜索 AA 模型" : "Search AA models"}</label>
            <Input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              disabled={pending}
              placeholder={isZh ? "按名称、slug 或厂商筛选…" : "Filter by name, slug, or creator…"}
            />
          </div>
          <div className="max-h-56 divide-y divide-slate-100 overflow-y-auto rounded-xl border border-slate-200">
            {filtered.map((item) => {
              const active = selectedKey === item.canonical_key;
              return (
                <button
                  key={item.canonical_key}
                  type="button"
                  onClick={() => setSelectedKey(item.canonical_key)}
                  className={`flex w-full items-center justify-between gap-2 px-3 py-2 text-left text-xs transition-colors ${active ? "bg-sky-50 text-sky-900" : "hover:bg-slate-50"}`}
                >
                  <div className="min-w-0">
                    <p className="truncate font-medium text-slate-800">{item.aa_name}</p>
                    <p className="truncate font-mono text-[11px] text-slate-500">
                      {item.canonical_key} · {item.aa_creator} · {item.selected_effort}
                    </p>
                  </div>
                  <Badge variant={active ? "default" : "secondary"} className="shrink-0 tabular-nums">
                    {item.score} ({item.raw_score.toFixed(1)})
                  </Badge>
                </button>
              );
            })}
            {filtered.length === 0 && (
              <div className="p-4 text-center text-xs text-slate-500">
                {models.length === 0
                  ? (isZh ? "尚未同步 AA 模型数据，请先配置并触发同步。" : "No AA models synced yet. Configure and run sync first.")
                  : (isZh ? "没有匹配的 AA 模型。" : "No matching AA models.")}
              </div>
            )}
          </div>
          {(setAlias.error || deleteAlias.error) && (
            <p role="alert" className="break-words text-xs text-red-600">
              {localizeBackendErrorMessage(setAlias.error ?? deleteAlias.error, isZh)}
            </p>
          )}
          <DialogFooter className="flex-wrap">
            {currentTargetKey && (
              <Button
                type="button"
                variant="ghost"
                disabled={pending}
                onClick={() => deleteAlias.mutate({ modelPrefix: prefixDraft.trim() }, { onSuccess: onClose })}
                className="mr-auto text-red-600 hover:text-red-700"
              >
                <Trash2 className="h-4 w-4" />
                {isZh ? "解除关联" : "Unbind"}
              </Button>
            )}
            <Button type="button" variant="secondary" disabled={pending} onClick={onClose}>
              {isZh ? "取消" : "Cancel"}
            </Button>
            <Button type="submit" disabled={pending || !prefixDraft.trim() || !selectedKey}>
              {setAlias.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
              {isZh ? "保存关联" : "Save binding"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
