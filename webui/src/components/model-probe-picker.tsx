import { useEffect, useMemo, useState } from "react";

import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { backend } from "@/lib/backend";
import { probeResultsForScope, type ProviderModelProbeRecord, type StoredModelProbeResult } from "@/lib/model-probe";
import { accountStateLabel, type AccountPoolView } from "@/lib/devin-account-pool";
import {
  canConfirmModelProbe,
  canProbeAccount,
  initialProbeAccountId,
  probePoolAccounts,
  selectedProbeAccount,
  supportsProbeAccounts,
  type ProbeAccountsState,
  type ProbeProvider,
} from "@/lib/model-probe-accounts";
import {
  buildProbeSelection,
  DEFAULT_MODEL_PROBE_PROMPT,
  loadProbePrompt,
  loadProbeSelection,
  MAX_MODEL_PROBE_PROMPT_CHARS,
  normalizeProbePrompt,
  partitionSavedSelection,
} from "@/lib/model-probe-selection";

/** Above this many selected models the confirm button turns into a warning. */
const LARGE_SELECTION = 20;

interface ModelProbePickerProps {
  open: boolean;
  provider: ProbeProvider | null;
  isZh: boolean;
  /** Status badges are read only from the selected account's namespace. */
  lastRecord?: ProviderModelProbeRecord;
  /**
   * Receives a non-empty, normalized selection and the trimmed probe message
   * (empty = built-in default) when the user confirms.
   */
  onConfirm: (models: string[], prompt: string, accountId?: string) => void;
  onCancel: () => void;
}

/**
 * One-shot picker for the models a probe run should hit. Nothing is probed
 * implicitly: the run covers exactly the checked catalog models plus the
 * extras box, and the selection is remembered per provider.
 */
export function ModelProbePicker({
  open,
  provider,
  isZh,
  lastRecord,
  onConfirm,
  onCancel,
}: ModelProbePickerProps) {
  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!value) onCancel();
      }}
    >
      <DialogContent className="max-h-[90vh] w-[min(92vw,560px)] overflow-y-auto">
        {open && provider && (
          <ModelProbePickerBody
            key={provider.id}
            provider={provider}
            isZh={isZh}
            lastRecord={lastRecord}
            onConfirm={onConfirm}
            onCancel={onCancel}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}

/**
 * Mounted only while the dialog is open — every open starts from a fresh,
 * unchecked state and re-fetches the catalog (no state is reset from inside
 * an effect; async callbacks own every state update).
 */
function ModelProbePickerBody({
  provider,
  isZh,
  lastRecord,
  onConfirm,
  onCancel,
}: Omit<ModelProbePickerProps, "open"> & { provider: ProbeProvider }) {
  const [catalog, setCatalog] = useState<string[]>([]);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [extrasText, setExtrasText] = useState("");
  const [search, setSearch] = useState("");
  // Remembered across providers; blank keeps the built-in "hi".
  const [promptText, setPromptText] = useState(loadProbePrompt);
  const requiresAccount = supportsProbeAccounts(provider);
  const [accountState, setAccountState] = useState<ProbeAccountsState>({ status: "loading" });
  const [accountId, setAccountId] = useState("");
  const account = selectedProbeAccount(accountState, accountId);

  useEffect(() => {
    if (!requiresAccount) return;
    const controller = new AbortController();
    void backend<AccountPoolView | null>("get_provider_account_pool", { id: provider.id }, { signal: controller.signal })
      .then((pool) => {
        if (controller.signal.aborted) return;
        const accounts = probePoolAccounts(pool, provider.id);
        setAccountState({ status: "ready", accounts });
        setAccountId(initialProbeAccountId(accounts));
      })
      .catch((error: unknown) => {
        if (!controller.signal.aborted) {
          setAccountState({ status: "error", error: error instanceof Error ? error.message : String(error) });
        }
      });
    return () => controller.abort();
  }, [provider.id, requiresAccount]);

  useEffect(() => {
    let stale = false;
    // Restore the remembered selection once the catalog settles, partitioning
    // it so one name occupies exactly one place.
    const restore = (list: string[]) => {
      const saved = loadProbeSelection(provider.id);
      const { checked: checkedNames, extras } = partitionSavedSelection(saved, list);
      setChecked(new Set(checkedNames));
      setExtrasText(extras.join("\n"));
    };
    void backend<string[]>("test_provider_models", { id: provider.id })
      .then((models) => {
        if (stale) return;
        const list = Array.isArray(models) ? models : [];
        setCatalog(list);
        restore(list);
      })
      .catch((error: unknown) => {
        if (stale) return;
        setCatalogError(error instanceof Error ? error.message : String(error));
        // A failing catalog fetch never disables the dialog — the extras box
        // stays usable so known ids can still be probed alone.
        restore([]);
      })
      .finally(() => {
        if (!stale) setLoading(false);
      });
    return () => {
      stale = true;
    };
  }, [provider.id]);

  const trimmedSearch = search.trim().toLowerCase();
  const filteredModels = useMemo(
    () => (trimmedSearch
      ? catalog.filter((model) => model.toLowerCase().includes(trimmedSearch))
      : catalog),
    [catalog, trimmedSearch],
  );
  const badgeByModel = useMemo(() => {
    const map = new Map<string, StoredModelProbeResult>();
    const results = requiresAccount && !account
      ? [] : probeResultsForScope(lastRecord, requiresAccount ? account?.account_id : undefined);
    for (const result of results) map.set(result.model, result);
    return map;
  }, [lastRecord, requiresAccount, account]);

  const selection = buildProbeSelection([...checked], extrasText);
  const count = selection.length;
  const warnLarge = count > LARGE_SELECTION;

  function toggleModel(model: string, on: boolean) {
    setChecked((prev) => {
      const next = new Set(prev);
      if (on) next.add(model);
      else next.delete(model);
      return next;
    });
  }

  /** Explicit bulk actions over the filtered rows only; clearing the search
   *  never changes what is checked. */
  function setFiltered(on: boolean) {
    setChecked((prev) => {
      const next = new Set(prev);
      for (const model of filteredModels) {
        if (on) next.add(model);
        else next.delete(model);
      }
      return next;
    });
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle>
          {isZh
            ? `选择要测试的模型 — ${provider.name}`
            : `Select models to probe — ${provider.name}`}
        </DialogTitle>
        <DialogDescription>
          {isZh
            ? "仅向选中的模型发送测试消息，未选中的模型不会被访问。"
            : "Sends the test message only to the selected models; unselected models are never touched."}
        </DialogDescription>
      </DialogHeader>

      <div className="space-y-3">
        {requiresAccount && (
          <div className="space-y-1 rounded-md border border-slate-200 p-3">
            <label htmlFor="model-probe-account" className="text-xs font-medium text-slate-700">
              {isZh ? "测试账号（必选）" : "Probe account (required)"}
            </label>
            <select
              id="model-probe-account"
              value={accountId}
              onChange={(event) => setAccountId(event.target.value)}
              disabled={accountState.status !== "ready" || accountState.accounts.length === 0}
              className="w-full rounded-md border border-slate-200 bg-white px-2 py-2 text-xs disabled:opacity-50"
            >
              <option value="">{isZh ? "请选择账号" : "Select an account"}</option>
              {accountState.status === "ready" && accountState.accounts.map((item) => (
                <option key={item.account_id} value={item.account_id} disabled={!canProbeAccount(item)}>
                  {item.label || item.account_id} · {item.account_id} · {accountStateLabel(item.status, isZh)}
                  {!item.is_enabled ? ` · ${accountStateLabel("disabled", isZh)}` : ""}
                </option>
              ))}
            </select>
            <p className="text-xs text-slate-500" role="status">
              {accountState.status === "loading"
                ? (isZh ? "正在加载账号…" : "Loading accounts…")
                : accountState.status === "error"
                  ? (isZh ? `账号加载失败：${accountState.error}` : `Failed to load accounts: ${accountState.error}`)
                  : accountState.accounts.length === 0
                    ? (isZh ? "没有账号，请先在供应商中添加并授权账号。" : "No accounts. Add and authorize an account in the provider first.")
                    : !accountState.accounts.some(canProbeAccount)
                      ? (isZh ? "没有已启用且已授权的账号。" : "No enabled, connected accounts.")
                      : account
                        ? (isZh ? `本次账号：${account.label || account.account_id}（${account.account_id}）。历史标记仅属于此账号。` : `This run: ${account.label || account.account_id} (${account.account_id}). History badges belong only to this account.`)
                        : (isZh ? "请选择一个已启用且已授权的账号；不会自动调度或回退到其他账号。" : "Choose an enabled, connected account; no automatic scheduling or fallback to another account.")}
            </p>
          </div>
        )}
        <div className="space-y-1">
          <label htmlFor="model-probe-prompt" className="text-xs text-slate-500">
            {isZh
              ? `测试消息（留空则使用默认的 "${DEFAULT_MODEL_PROBE_PROMPT}"）`
              : `Test message (leave blank for the default "${DEFAULT_MODEL_PROBE_PROMPT}")`}
          </label>
          <textarea
            id="model-probe-prompt"
            value={promptText}
            onChange={(event) => setPromptText(event.target.value)}
            rows={2}
            maxLength={MAX_MODEL_PROBE_PROMPT_CHARS}
            className="w-full rounded-md border border-slate-200 px-3 py-2 text-xs"
            placeholder={DEFAULT_MODEL_PROBE_PROMPT}
          />
        </div>

        <div className="flex items-center gap-2">
          <Input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder={isZh ? "搜索模型…" : "Search models…"}
            className="h-8"
          />
          <Button type="button" variant="outline" size="sm" onClick={() => setFiltered(true)}>
            {isZh ? "全选过滤结果" : "Select filtered"}
          </Button>
          <Button type="button" variant="outline" size="sm" onClick={() => setFiltered(false)}>
            {isZh ? "清空过滤结果" : "Clear filtered"}
          </Button>
        </div>

        {catalogError && (
          <div className="rounded-md border border-red-200 bg-red-50 px-3 py-2 text-xs text-red-600">
            {isZh
              ? `模型目录获取失败：${catalogError}（仍可只测试下方补录的模型）`
              : `Model catalog fetch failed: ${catalogError} (extras below still work)`}
          </div>
        )}

        <div className="h-56 overflow-y-auto rounded-md border border-slate-200 p-2">
          {loading ? (
            <p className="p-2 text-xs text-slate-400">
              {isZh ? "正在获取模型列表…" : "Fetching model list…"}
            </p>
          ) : filteredModels.length === 0 ? (
            <p className="p-2 text-xs text-slate-400">
              {catalog.length === 0
                ? (isZh ? "没有可勾选的模型" : "No models to check")
                : (isZh ? "没有匹配的模型" : "No matching models")}
            </p>
          ) : (
            filteredModels.map((model) => {
              const result = badgeByModel.get(model);
              return (
                <div
                  key={model}
                  className="flex cursor-pointer items-center gap-2 rounded px-2 py-1 hover:bg-slate-50"
                  onClick={() => toggleModel(model, !checked.has(model))}
                >
                  <Checkbox
                    checked={checked.has(model)}
                    onCheckedChange={(value) => toggleModel(model, value === true)}
                    onClick={(event) => event.stopPropagation()}
                  />
                  <span className="min-w-0 flex-1 truncate font-mono text-xs text-slate-700">
                    {model}
                  </span>
                  <ProbeBadge result={result} isZh={isZh} />
                </div>
              );
            })
          )}
        </div>

        <div className="space-y-1">
          <p className="text-xs text-slate-500">
            {isZh
              ? "补录模型（一行一个或逗号分隔；目录外的模型 id 也可测试）"
              : "Extra models (one per line or comma separated; catalog-external ids are probed too)"}
          </p>
          <textarea
            value={extrasText}
            onChange={(event) => setExtrasText(event.target.value)}
            rows={3}
            className="w-full rounded-md border border-slate-200 px-3 py-2 font-mono text-xs"
            placeholder={isZh ? "例如 gpt-5.2-mini" : "e.g. gpt-5.2-mini"}
          />
        </div>
      </div>

      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          {isZh ? "取消" : "Cancel"}
        </Button>
        <Button
          type="button"
          onClick={() => {
            if (!canConfirmModelProbe(provider, count, accountState, accountId)) return;
            onConfirm(selection, normalizeProbePrompt(promptText), requiresAccount ? account?.account_id : undefined);
          }}
          disabled={!canConfirmModelProbe(provider, count, accountState, accountId)}
          variant={warnLarge ? "outline" : "default"}
          className={
            warnLarge
              ? "border-amber-400 bg-amber-50 text-amber-700 hover:bg-amber-100"
              : undefined
          }
        >
          {isZh ? `探测 ${count} 个模型` : `Probe ${count} models`}
        </Button>
      </DialogFooter>
    </>
  );
}

/** Lightweight last-run marker: green ✓ / red ✗ with details in the title. */
function ProbeBadge({ result, isZh }: { result?: StoredModelProbeResult; isZh: boolean }) {
  if (!result) return null;
  const when = result.run_at ? new Date(result.run_at).toLocaleString() : "";
  const suffix = when ? ` · ${when}` : "";
  if (result.success) {
    return (
      <span
        title={(isZh ? `上次成功 · ${result.latency_ms}ms` : `Last run ok · ${result.latency_ms}ms`) + suffix}
        className="text-xs text-green-600"
      >
        ✓
      </span>
    );
  }
  const reason = result.error ?? (isZh ? "上次失败" : "Last run failed");
  return (
    <span title={reason + suffix} className="text-xs text-red-500">
      ✗
    </span>
  );
}
