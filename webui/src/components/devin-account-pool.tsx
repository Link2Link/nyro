import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { backend } from "@/lib/backend";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { useLocale } from "@/lib/i18n";
import { accountStateLabel, canReauthorizeLegacyProvider, type AccountPoolView, type AccountQuotaWindow, type OAuthAccountUpdate, type ProviderAccountView } from "@/lib/devin-account-pool";
import type { Provider } from "@/lib/types";
import type { OAuthSessionIntent } from "@/lib/use-oauth-session";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { DevinPoolUsage, ProviderUsageFooter } from "@/components/provider-usage";
import { DevinPoolSettings } from "@/components/devin-pool-settings";
import { DevinAccountLogin } from "@/components/devin-account-login";

function useAccountPool(providerId: string) {
  return useQuery<AccountPoolView>({
    queryKey: ["provider-account-pool", providerId],
    queryFn: () => backend("get_provider_account_pool", { id: providerId }),
    staleTime: 30_000, refetchInterval: 60_000, refetchOnWindowFocus: false,
  });
}

function AccountQuota({ window }: { window: AccountQuotaWindow | null }) {
  if (!window) return <span>—</span>;
  return <div className="space-y-1 tabular-nums">
    <span>{window.remaining_percent}%</span>
    {window.resets_at && <time className="block text-[10px] text-slate-400" dateTime={window.resets_at}>{new Date(window.resets_at).toLocaleString()}</time>}
  </div>;
}

function AccountRow({ account, showDaily, pending, canLogin, onUpdate, onLogin, onRefresh, onRemove }: {
  account: ProviderAccountView; showDaily: boolean; pending: boolean; canLogin: boolean;
  onUpdate: (update: OAuthAccountUpdate) => void; onLogin: () => void; onRefresh: () => void; onRemove: () => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [label, setLabel] = useState(account.label);
  const [priority, setPriority] = useState(account.priority);
  const changed = label !== account.label || priority !== account.priority;
  const disconnected = account.status === "disconnected";
  return <tr className="border-t border-slate-200 align-top">
    <td className="min-w-44 space-y-1 p-2">
      <Input aria-label={isZh ? "账号别名" : "Account label"} value={label} maxLength={120} disabled={pending || disconnected} onChange={(event) => setLabel(event.target.value)} />
      <div className="break-all text-[10px] text-slate-400">{account.account_id} · v{account.status_version}</div>
      <div className="text-[10px] text-slate-500">{account.verified_plan?.toUpperCase() ?? (isZh ? "套餐未验证" : "Plan unverified")} · {account.identity_verified ? (isZh ? "身份已验证" : "Identity verified") : (isZh ? "身份未确认" : "Identity unverified")}</div>
      <label className="flex items-center gap-2 text-[10px] text-slate-500"><span>{isZh ? "优先级（越小越优先）" : "Priority (lower first)"}</span>
        <Input className="w-20" aria-label={isZh ? "账号优先级" : "Account priority"} type="number" step={1} value={priority} disabled={pending || disconnected} onChange={(event) => setPriority(Number(event.target.value))} />
      </label>
      {changed && <Button type="button" variant="secondary" disabled={pending || !label.trim() || !Number.isInteger(priority) || priority < -2147483648 || priority > 2147483647} onClick={() => onUpdate({ label: label.trim(), priority })}>{isZh ? "保存" : "Save"}</Button>}
    </td>
    <td className="space-y-1 p-2"><p>{accountStateLabel(account.status, isZh)}</p><p className="text-slate-500">{accountStateLabel(account.scheduling, isZh)}</p><p>{account.is_enabled ? (isZh ? "已启用" : "Enabled") : (isZh ? "已停用" : "Disabled")}</p></td>
    {showDaily && <td className="p-2"><AccountQuota window={account.daily} /></td>}
    <td className="p-2"><AccountQuota window={account.weekly} /></td>
    <td className="space-y-1 p-2"><p>{account.reserve_usd == null ? "—" : `$${account.reserve_usd.toFixed(2)}`}</p>
      <p className={account.stale ? "text-amber-700" : "text-slate-400"}>{account.stale ? (isZh ? "旧数据 / 未知" : "Stale / unknown") : (isZh ? "已观察" : "Observed")}</p>
      {account.observed_at != null && <time className="text-[10px] text-slate-400" dateTime={new Date(account.observed_at).toISOString()}>{new Date(account.observed_at).toLocaleString()}</time>}
    </td>
    <td className="min-w-36 p-2"><div className="flex flex-wrap gap-1">
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={onRefresh}>{isZh ? "刷新用量" : "Refresh usage"}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected || !canLogin} onClick={onLogin}>{isZh ? "重登" : "Re-authorize"}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={() => onUpdate({ is_enabled: !account.is_enabled })}>{account.is_enabled ? (isZh ? "停用" : "Disable") : (isZh ? "启用" : "Enable")}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={onRemove}>{isZh ? "移除" : "Remove"}</Button>
    </div></td>
  </tr>;
}

export function DevinAccountPoolPanel({ provider }: { provider: Provider }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const query = useAccountPool(provider.id);
  const qc = useQueryClient();
  const [login, setLogin] = useState<{ intent: OAuthSessionIntent; label: string } | null>(null);
  const [remove, setRemove] = useState<ProviderAccountView | null>(null);
  function invalidate() {
    for (const key of ["provider-account-pool", "provider-usage", "provider-oauth-status"]) void qc.invalidateQueries({ queryKey: [key, provider.id] });
    void qc.invalidateQueries({ queryKey: ["providers"] });
  }
  const mutation = useMutation({
    mutationFn: ({ command, args }: { command: string; args?: Record<string, unknown> }) => backend(command, { id: provider.id, ...args }),
    // Refresh after conflict as well, so stale CAS versions are not silently reused.
    onSettled: invalidate,
  });
  const pool = query.data;
  const pending = mutation.isPending || Boolean(login);
  const error = mutation.error || query.error;
  const act = (command: string, args?: Record<string, unknown>) => mutation.mutate({ command, args });
  if (!pool) return <div role={error ? "alert" : "status"} className="text-xs text-slate-500">
    {error ? localizeBackendErrorMessage(error, isZh) : (isZh ? "正在读取账号池…" : "Loading account pool…")}
    {error && <Button type="button" variant="secondary" onClick={() => void query.refetch()}>{isZh ? "重试" : "Retry"}</Button>}
  </div>;
  return <section className="space-y-3 rounded-xl border border-slate-200 bg-slate-50 p-3">
    <h3 className="text-sm font-semibold">{isZh ? "Devin 账号池" : "Devin account pool"}</h3>
    <DevinPoolUsage pool={pool} />
    {error && <p role="alert" className="text-xs text-red-600">{localizeBackendErrorMessage(error, isZh)}</p>}
    <details open={!pool.config}><summary className="cursor-pointer text-xs font-medium">{isZh ? "套餐与调度设置" : "Plan and scheduling settings"}</summary>
      <div className="pt-3"><DevinPoolSettings key={JSON.stringify(pool.config)} config={pool.config}
        hasAccounts={pool.accounts.some((account) => account.status !== "disconnected")} pending={pending}
        onSave={(config) => act("configure_provider_account_pool", { config })} /></div>
    </details>
    {!pool.config && <p className="text-xs text-slate-600">{isZh ? "原单账号可继续重登，无需选择 Pro/Max。只有扩展账号池才需要选择并验证套餐。" : "The legacy single account can still sign in without selecting Pro/Max. A verified plan is required only to enable the account pool."}</p>}
    <div className="flex flex-wrap gap-2">
      {canReauthorizeLegacyProvider(pool, provider.id) && <Button type="button" variant="secondary" disabled={pending} onClick={() => {
        mutation.reset();
        setLogin({ intent: { kind: "reauthorize_legacy_provider", providerId: provider.id, useProxy: provider.use_proxy }, label: "" });
      }}>{isZh ? "原单账号重登" : "Re-authorize legacy account"}</Button>}
      <Button type="button" disabled={!pool.config || pending} onClick={() => { mutation.reset(); setLogin({ intent: { kind: "add_account", providerId: provider.id }, label: "" }); }}>{isZh ? "添加账号" : "Add account"}</Button>
      <Button type="button" variant="secondary" disabled={pending} onClick={() => act("refresh_provider_account_pool_usage")}><RefreshCw className={`mr-1 h-3 w-3 ${mutation.isPending ? "animate-spin" : ""}`} />{isZh ? "刷新全池用量" : "Refresh pool usage"}</Button>
    </div>
    {login && <DevinAccountLogin key={login.intent.kind === "reauthorize_account" ? `${login.intent.accountId}:${login.intent.expectedVersion}` : `${login.intent.kind}:${login.intent.providerId}`}
      intent={login.intent} initialLabel={login.label} onCancel={() => setLogin(null)} onBound={() => { setLogin(null); invalidate(); }} />}
    <div className="overflow-x-auto"><table className="w-full text-left text-xs">
      <thead className="text-slate-500"><tr>
        <th className="p-2">{isZh ? "账号 / 版本 / 套餐" : "Account / version / plan"}</th>
        <th className="p-2">{isZh ? "授权 / 调度" : "Authorization / scheduling"}</th>
        {pool.config?.plan !== "max" && <th className="p-2">{isZh ? "日剩余" : "Daily remaining"}</th>}
        <th className="p-2">{isZh ? "周剩余" : "Weekly remaining"}</th><th className="p-2">{isZh ? "付费余额 / 观察时间" : "Paid balance / observed"}</th><th className="p-2">{isZh ? "操作" : "Actions"}</th>
      </tr></thead>
      <tbody>{pool.accounts.map((account) => {
        const target = { accountId: account.account_id, expectedVersion: account.status_version };
        return <AccountRow key={`${account.account_id}:${account.status_version}`} account={account} showDaily={pool.config?.plan !== "max"} pending={pending} canLogin={Boolean(pool.config)}
          onUpdate={(update) => act("update_provider_account", { ...target, update })}
          onRefresh={() => act("refresh_provider_account_usage", { accountId: account.account_id })}
          onRemove={() => setRemove(account)}
          onLogin={() => { mutation.reset(); setLogin({ intent: { kind: "reauthorize_account", providerId: provider.id, ...target }, label: account.label }); }} />;
      })}</tbody>
    </table></div>
    {pool.accounts.length === 0 && <p className="text-xs text-slate-500">{isZh ? "尚无账号" : "No accounts yet"}</p>}
    <ConfirmDialog open={Boolean(remove)} onOpenChange={(open) => { if (!open) setRemove(null); }}
      title={isZh ? `移除 ${remove?.label ?? ""}？` : `Remove ${remove?.label ?? ""}?`}
      description={isZh ? "仅清除此账号的凭据，其他账号不受影响。保留断开记录以阻止旧回调恢复授权。" : "Only this account's credentials are cleared; other accounts are unaffected. A disconnected record is retained to prevent stale callbacks from restoring authorization."}
      confirmText={isZh ? "移除账号" : "Remove account"} cancelText={isZh ? "取消" : "Cancel"}
      onConfirm={() => { if (remove && !mutation.isPending) act("disconnect_provider_account", { accountId: remove.account_id, expectedVersion: remove.status_version }); setRemove(null); }} />
  </section>;
}

/** One Provider card stays one routing target; the pool replaces only its usage footer. */
export function DevinAccountPoolCard({ provider }: { provider: Provider }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const query = useAccountPool(provider.id);
  const [open, setOpen] = useState(false);
  return <div className="mt-2 space-y-2">
    {query.data?.config ? <DevinPoolUsage pool={query.data} /> : query.data ? <>
      <ProviderUsageFooter provider={provider} compact />
      <p className="text-xs text-amber-700">{isZh ? "扩展账号池需要选择并验证套餐；原单账号仍可正常使用和重登。" : "Select and verify a plan to enable the pool; the legacy single account can still be used and re-authorized."}</p>
    </> : <p className="text-xs text-slate-500">{query.error ? localizeBackendErrorMessage(query.error, isZh) : (isZh ? "读取账号池…" : "Loading account pool…")}</p>}
    <Button type="button" variant="secondary" onClick={() => setOpen(!open)} aria-expanded={open}>{open ? (isZh ? "收起账号" : "Hide accounts") : (isZh ? "管理账号" : "Manage accounts")}</Button>
    {open && <DevinAccountPoolPanel provider={provider} />}
  </div>;
}
