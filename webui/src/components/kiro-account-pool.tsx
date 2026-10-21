import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { backend } from "@/lib/backend";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { useLocale } from "@/lib/i18n";
import { useUsageClock } from "@/lib/use-usage-clock";
import { accountStateLabel, canReauthorizeLegacyProvider, type AccountPoolView, type OAuthAccountUpdate, type ProviderAccountView } from "@/lib/devin-account-pool";
import type { Provider } from "@/lib/types";
import type { OAuthSessionIntent } from "@/lib/use-oauth-session";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { AccountPoolSettings } from "@/components/devin-pool-settings";
import { KiroAccountLogin } from "@/components/kiro-account-login";
import { ProviderUsageFooter } from "@/components/provider-usage";
import { UsageProgress } from "@/components/usage-progress";

function useAccountPool(providerId: string) {
  return useQuery<AccountPoolView>({
    queryKey: ["provider-account-pool", providerId],
    queryFn: () => backend("get_provider_account_pool", { id: providerId }),
    staleTime: 30_000, refetchInterval: 60_000, refetchOnWindowFocus: false,
  });
}

function useInvalidatePool(providerId: string) {
  const qc = useQueryClient();
  return () => Promise.all([
    ...["provider-account-pool", "provider-usage", "provider-oauth-status"].map((key) => qc.invalidateQueries({ queryKey: [key, providerId] })),
    qc.invalidateQueries({ queryKey: ["providers"] }),
  ]);
}

function kiroStateLabel(state: string, isZh: boolean) {
  if (state === "reserve") return isZh ? "付费超额" : "Paid overage";
  if (state === "reserve_only") return isZh ? "仅付费超额可用" : "Paid overage only";
  return accountStateLabel(state, isZh);
}

function ResetTime({ value, isZh }: { value: string | null; isZh: boolean }) {
  if (!value || !Number.isFinite(Date.parse(value))) return null;
  return <p className="text-[10px] text-slate-500">{isZh ? "重置 / 到期" : "Resets / expires"}: <time dateTime={value}>{new Date(value).toLocaleString()}</time></p>;
}

/** Show upstream resources separately: no Pro/Max weighting, synthetic windows or eligibility rules. */
export function KiroAccountUsage({ account, isZh, compact = false }: { account: ProviderAccountView; isZh: boolean; compact?: boolean }) {
  const usage = account.kiro_usage;
  const now = useUsageClock();
  const kindLabels: Record<string, string> = {
    base: isZh ? "基础额度" : "Base allowance",
    bonus: isZh ? "赠送额度" : "Bonus",
    free_trial: isZh ? "免费试用" : "Free trial",
  };
  return <div className="min-w-48 space-y-2 text-xs">
    {!compact && <>
      <p className="font-medium">{usage?.plan ?? (isZh ? "套餐未知" : "Plan unknown")}</p>
      {usage?.email && <p className="break-all text-slate-500">{usage.email}</p>}
    </>}
    {!usage?.items.length && <p className="text-slate-500">{isZh ? "用量未知" : "Usage unknown"}</p>}
    {usage?.items.map((item, index) => {
      const valid = Number.isFinite(item.used) && item.used >= 0 && Number.isFinite(item.limit) && item.limit > 0;
      // Historical snapshots can retain expired bonuses/trials. Annotate only;
      // the backend alone determines scheduling and current resource capacity.
      const expired = (item.kind === "bonus" || item.kind === "free_trial") && item.resets_at !== null && Date.parse(item.resets_at) <= now;
      const usedPercent = valid && !expired ? item.used / item.limit * 100 : null;
      const name = `${item.name || item.resource} · ${kindLabels[item.kind] ?? item.kind}`;
      return <div key={`${item.resource}:${item.kind}:${index}`} className={`space-y-1 ${expired ? "text-slate-400" : ""}`}>
        <p className={expired ? "text-slate-400" : "text-slate-600"}>{name}{expired && <span> · {isZh ? "已到期（历史用量）" : "Expired (historical usage)"}</span>}</p>
        {!compact && <p className="text-[10px] text-slate-400">{isZh ? "资源" : "Resource"}: {item.resource}</p>}
        <p className="tabular-nums">{isZh ? "已用" : "Used"} {Number.isFinite(item.used) && item.used >= 0 ? item.used.toLocaleString() : "—"} / {Number.isFinite(item.limit) && item.limit >= 0 ? item.limit.toLocaleString() : "—"}</p>
        {usedPercent !== null && <UsageProgress usedPercent={usedPercent} label={`${name} ${isZh ? "已用额度" : "used quota"}`} isZh={isZh} />}
        {!compact && <ResetTime value={item.resets_at} isZh={isZh} />}
      </div>;
    })}
    {!compact && usage && <>
      <ResetTime value={usage.resets_at} isZh={isZh} />
      <p className={usage.overage_enabled ? "text-amber-700" : "text-slate-500"}>{usage.overage_enabled
        ? (isZh ? "上游超额计费已开启" : "Upstream overage enabled")
        : (isZh ? "上游超额计费未开启" : "Upstream overage disabled")}</p>
    </>}
    <p className={account.stale ? "text-amber-700" : "text-slate-400"}>{account.stale ? (isZh ? "旧数据 / 未知" : "Stale / unknown") : (isZh ? "已观察" : "Observed")}</p>
    {!compact && account.observed_at != null && Number.isFinite(new Date(account.observed_at).getTime()) && <time className="text-[10px] text-slate-400" dateTime={new Date(account.observed_at).toISOString()}>{new Date(account.observed_at).toLocaleString()}</time>}
  </div>;
}

function PoolSummary({ pool, isZh }: { pool: AccountPoolView; isZh: boolean }) {
  return <div className="space-y-1 text-xs text-slate-600">
    <p>{isZh ? `${pool.counts.total} 个账号 · ${pool.counts.available} 个可用` : `${pool.counts.total} accounts · ${pool.counts.available} available`} · {kiroStateLabel(pool.readiness, isZh)}</p>
    <p>{isZh ? `已停用 ${pool.counts.disabled} · 已断开 ${pool.counts.disconnected} · 需要登录 ${pool.counts.auth_required} · 额度耗尽 ${pool.counts.quota_exhausted}` : `Disabled ${pool.counts.disabled} · Disconnected ${pool.counts.disconnected} · Needs login ${pool.counts.auth_required} · Quota exhausted ${pool.counts.quota_exhausted}`}</p>
  </div>;
}

function AccountRow({ account, pending, canLogin, onUpdate, onLogin, onRefresh, onRemove, isZh }: {
  account: ProviderAccountView; pending: boolean; canLogin: boolean; isZh: boolean;
  onUpdate: (update: OAuthAccountUpdate) => void; onLogin: () => void; onRefresh: () => void; onRemove: () => void;
}) {
  const [label, setLabel] = useState(account.label);
  const [priority, setPriority] = useState(account.priority);
  const changed = label !== account.label || priority !== account.priority;
  const disconnected = account.status === "disconnected";
  return <tr className="border-t border-slate-200 align-top">
    <td className="min-w-44 space-y-2 p-2">
      <Input aria-label={isZh ? "账号别名" : "Account label"} value={label} maxLength={120} disabled={pending || disconnected} onChange={(event) => setLabel(event.target.value)} />
      <div className="break-all text-[10px] text-slate-400">{account.account_id} · v{account.status_version}</div>
      <label className="flex items-center gap-2 text-[10px] text-slate-500"><span>{isZh ? "优先级（越小越优先）" : "Priority (lower first)"}</span>
        <Input className="w-20" aria-label={isZh ? "账号优先级" : "Account priority"} type="number" step={1} value={priority} disabled={pending || disconnected} onChange={(event) => setPriority(Number(event.target.value))} />
      </label>
      {changed && <Button type="button" variant="secondary" disabled={pending || disconnected || !label.trim() || !Number.isInteger(priority) || priority < -2147483648 || priority > 2147483647} onClick={() => onUpdate({ label: label.trim(), priority })}>{isZh ? "保存" : "Save"}</Button>}
    </td>
    <td className="space-y-1 p-2"><p>{accountStateLabel(account.status, isZh)}</p><p className="text-slate-500">{kiroStateLabel(account.scheduling, isZh)}</p><p>{account.is_enabled ? (isZh ? "已启用" : "Enabled") : (isZh ? "已停用" : "Disabled")}</p></td>
    <td className="p-2"><KiroAccountUsage account={account} isZh={isZh} /></td>
    <td className="min-w-36 p-2"><div className="flex flex-wrap gap-1">
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={onRefresh}>{isZh ? "刷新用量" : "Refresh usage"}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected || !canLogin} onClick={onLogin}>{isZh ? "重新授权 / 导入" : "Re-authorize / import"}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={() => onUpdate({ is_enabled: !account.is_enabled })}>{account.is_enabled ? (isZh ? "停用" : "Disable") : (isZh ? "启用" : "Enable")}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={onRemove}>{isZh ? "移除" : "Remove"}</Button>
    </div></td>
  </tr>;
}

export function KiroAccountPoolPanel({ provider }: { provider: Provider }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const query = useAccountPool(provider.id);
  const invalidate = useInvalidatePool(provider.id);
  const [login, setLogin] = useState<{ intent: OAuthSessionIntent; label: string } | null>(null);
  const [remove, setRemove] = useState<ProviderAccountView | null>(null);
  const mutation = useMutation({
    mutationFn: ({ command, args }: { command: string; args?: Record<string, unknown> }) => backend(command, { id: provider.id, ...args }),
    // Re-read CAS versions after errors too, including partial refresh failures.
    onSettled: invalidate,
  });
  const pool = query.data;
  const pending = mutation.isPending || Boolean(login) || Boolean(remove);
  const error = mutation.error || query.error;
  const act = (command: string, args?: Record<string, unknown>) => mutation.mutate({ command, args });
  if (!pool) return <div role={error ? "alert" : "status"} className="text-xs text-slate-500">
    {error ? localizeBackendErrorMessage(error, isZh) : (isZh ? "正在读取账号池…" : "Loading account pool…")}
    {error && <Button type="button" variant="secondary" onClick={() => void query.refetch()}>{isZh ? "重试" : "Retry"}</Button>}
  </div>;
  return <section className="space-y-3 rounded-xl border border-slate-200 bg-slate-50 p-3">
    <h3 className="text-sm font-semibold">{isZh ? "Kiro 账号池" : "Kiro account pool"}</h3>
    <PoolSummary pool={pool} isZh={isZh} />
    <p className="text-xs text-slate-500">{isZh ? "网关会使用已保存的 refresh token 自动续期；静态 API Key 不支持续期。仅在凭证失效时重新授权或导入。" : "The gateway automatically refreshes credentials using stored refresh tokens; static API keys cannot be refreshed. Re-authorize or import when credentials become invalid."}</p>
    {error && <p role="alert" className="text-xs text-red-600">{localizeBackendErrorMessage(error, isZh)}</p>}
    <details open={!pool.config}><summary className="cursor-pointer text-xs font-medium">{isZh ? "账号池调度设置" : "Pool scheduling settings"}</summary>
      <div className="pt-3"><AccountPoolSettings key={JSON.stringify(pool.config)} vendor="kiro" config={pool.config} pending={pending}
        onSave={(config) => act("configure_provider_account_pool", { config })} /></div>
    </details>
    <div className="flex flex-wrap gap-2">
      {canReauthorizeLegacyProvider(pool, provider.id) && <Button type="button" variant="secondary" disabled={pending} onClick={() => {
        mutation.reset();
        setLogin({ intent: { kind: "reauthorize_legacy_provider", providerId: provider.id, useProxy: provider.use_proxy }, label: "" });
      }}>{isZh ? "原单账号重新授权 / 导入" : "Re-authorize / import legacy account"}</Button>}
      <Button type="button" disabled={!pool.config || pending} onClick={() => { mutation.reset(); setLogin({ intent: { kind: "add_account", providerId: provider.id }, label: "" }); }}>{isZh ? "添加账号" : "Add account"}</Button>
      <Button type="button" variant="secondary" disabled={pending} onClick={() => act("refresh_provider_account_pool_usage")}><RefreshCw className={`mr-1 h-3 w-3 ${mutation.isPending ? "animate-spin" : ""}`} />{isZh ? "刷新全池用量" : "Refresh pool usage"}</Button>
    </div>
    {login && <KiroAccountLogin key={login.intent.kind === "reauthorize_account" ? `${login.intent.accountId}:${login.intent.expectedVersion}` : `${login.intent.kind}:${login.intent.providerId}`}
      intent={login.intent} initialLabel={login.label} onCancel={() => setLogin(null)} onBound={() => { setLogin(null); void invalidate(); }} />}
    <div className="overflow-x-auto"><table className="w-full text-left text-xs">
      <thead className="text-slate-500"><tr>
        <th className="p-2">{isZh ? "账号 / 版本" : "Account / version"}</th><th className="p-2">{isZh ? "授权 / 调度" : "Authorization / scheduling"}</th>
        <th className="p-2">{isZh ? "套餐 / 资源用量" : "Plan / resource usage"}</th><th className="p-2">{isZh ? "操作" : "Actions"}</th>
      </tr></thead>
      <tbody>{pool.accounts.map((account) => {
        const target = { accountId: account.account_id, expectedVersion: account.status_version };
        return <AccountRow key={`${account.account_id}:${account.status_version}`} account={account} isZh={isZh} pending={pending} canLogin={Boolean(pool.config)}
          onUpdate={(update) => act("update_provider_account", { ...target, update })}
          onRefresh={() => act("refresh_provider_account_usage", { accountId: account.account_id })}
          onRemove={() => setRemove(account)}
          onLogin={() => { mutation.reset(); setLogin({ intent: { kind: "reauthorize_account", providerId: provider.id, ...target }, label: account.label }); }} />;
      })}</tbody>
    </table></div>
    {pool.accounts.length === 0 && <p className="text-xs text-slate-500">{isZh ? "尚无账号" : "No accounts yet"}</p>}
    <ConfirmDialog open={Boolean(remove)} onOpenChange={(open) => { if (!open) setRemove(null); }}
      title={isZh ? `移除 ${remove?.label ?? ""}？` : `Remove ${remove?.label ?? ""}?`}
      description={isZh ? "仅清除此账号的凭据，其他账号不受影响。保留断开记录以阻止旧回调恢复授权；需要时请添加新账号。" : "Only this account's credentials are cleared; other accounts are unaffected. A disconnected record prevents stale callbacks from restoring authorization. Add a new account when needed."}
      confirmText={isZh ? "移除账号" : "Remove account"} cancelText={isZh ? "取消" : "Cancel"}
      onConfirm={() => { if (remove && !mutation.isPending) act("disconnect_provider_account", { accountId: remove.account_id, expectedVersion: remove.status_version }); setRemove(null); }} />
  </section>;
}

export function KiroAccountPoolCard({ provider }: { provider: Provider }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const query = useAccountPool(provider.id);
  const [open, setOpen] = useState(false);
  const invalidate = useInvalidatePool(provider.id);
  const refresh = useMutation({
    mutationFn: () => backend("refresh_provider_account_pool_usage", { id: provider.id }),
    onSettled: invalidate,
  });
  return <div className="mt-2 space-y-2">
    {query.data?.config ? <div className="space-y-2 rounded-lg border border-slate-200 p-3">
      <PoolSummary pool={query.data} isZh={isZh} />
      {!open && query.data.accounts.filter((account) => account.status !== "disconnected").map((account) => <div key={account.account_id} className="space-y-1 border-t border-slate-100 pt-2">
        <p className="text-xs font-semibold">{account.label} · {account.kiro_usage?.plan ?? (isZh ? "套餐未知" : "Plan unknown")} · {kiroStateLabel(account.scheduling, isZh)}</p>
        <KiroAccountUsage account={account} isZh={isZh} compact />
      </div>)}
      <Button type="button" variant="secondary" disabled={refresh.isPending} aria-busy={refresh.isPending} onClick={() => refresh.mutate()}><RefreshCw className={`mr-1 h-3 w-3 ${refresh.isPending ? "animate-spin" : ""}`} />{isZh ? "刷新全池用量" : "Refresh pool usage"}</Button>
    </div> : query.data ? <>
      <ProviderUsageFooter provider={provider} />
      <p className="text-xs text-amber-700">{isZh ? "保存账号池调度设置即可添加更多 Kiro 账号；原单账号仍可使用和重新授权。" : "Save pool scheduling settings to add more Kiro accounts; the legacy account can still be used and re-authorized."}</p>
    </> : <p className="text-xs text-slate-500">{query.error ? localizeBackendErrorMessage(query.error, isZh) : (isZh ? "读取账号池…" : "Loading account pool…")}</p>}
    {refresh.error && <p role="alert" className="text-xs text-red-600">{localizeBackendErrorMessage(refresh.error, isZh)}</p>}
    <Button type="button" variant="secondary" onClick={() => setOpen(!open)} aria-expanded={open}>{open ? (isZh ? "收起账号" : "Hide accounts") : (isZh ? "管理账号" : "Manage accounts")}</Button>
    {open && <KiroAccountPoolPanel provider={provider} />}
  </div>;
}
