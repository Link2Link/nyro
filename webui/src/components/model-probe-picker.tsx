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
import type { StoredModelProbeResult } from "@/lib/model-probe";
import {
  buildProbeSelection,
  loadProbeSelection,
  partitionSavedSelection,
} from "@/lib/model-probe-selection";

/** Above this many selected models the confirm button turns into a warning. */
const LARGE_SELECTION = 20;

interface ModelProbePickerProps {
  open: boolean;
  provider: { id: string; name: string } | null;
  isZh: boolean;
  /** Latest persisted probe results — rendered as status badges only. */
  lastResults?: StoredModelProbeResult[];
  /** Receives a non-empty, normalized selection when the user confirms. */
  onConfirm: (models: string[]) => void;
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
  lastResults,
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
      <DialogContent className="w-[min(92vw,560px)]">
        {provider && (
          <ModelProbePickerBody
            key={provider.id}
            provider={provider}
            isZh={isZh}
            lastResults={lastResults}
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
  lastResults,
  onConfirm,
  onCancel,
}: Omit<ModelProbePickerProps, "open"> & { provider: { id: string; name: string } }) {
  const [catalog, setCatalog] = useState<string[]>([]);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [extrasText, setExtrasText] = useState("");
  const [search, setSearch] = useState("");

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
    for (const result of lastResults ?? []) map.set(result.model, result);
    return map;
  }, [lastResults]);

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
            ? "仅向选中的模型发送 \"hi\"，未选中的模型不会被访问。"
            : "Sends \"hi\" only to the selected models; unselected models are never touched."}
        </DialogDescription>
      </DialogHeader>

      <div className="space-y-3">
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
          onClick={() => onConfirm(selection)}
          disabled={count === 0}
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
