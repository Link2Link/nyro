import { useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { backend } from "@/lib/backend";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { useLocale } from "@/lib/i18n";
import { useUsageClock } from "@/lib/use-usage-clock";
import { looksLikeCredential } from "@/lib/kiro-pool-usage";
import { tierLabel } from "@/lib/provider-usage";
import { steadyPacePercent, utilizationColor } from "@/lib/usage-pace";
import { accountStateLabel, canReauthorizeLegacyProvider, type AccountPoolView, type OAuthAccountUpdate, type ProviderAccountView } from "@/lib/devin-account-pool";
import type { Provider } from "@/lib/types";
import type { OAuthSessionIntent } from "@/lib/use-oauth-session";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { AccountPoolSettings } from "@/components/devin-pool-settings";
import { ClaudeAccountLogin } from "@/components/claude-account-login";
import { ProviderUsageFooter } from "@/components/provider-usage";
import { UsageProgress } from "@/components/usage-progress";
import { PoolRefreshButton } from "@/components/pool-refresh-button";
import { AccountUsageSummary } from "@/components/account-usage-summary";
import { useAccountUsageStats } from "@/lib/account-usage-stats";

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
    ...["provider-account-pool", "provider-account-usage-stats", "provider-usage", "provider-oauth-status"].map((key) => qc.invalidateQueries({ queryKey: [key, providerId] })),
    qc.invalidateQueries({ queryKey: ["providers"] }),
  ]);
}

/** Percentages belong to a single account and window, never to pooled capacity. */
export function ClaudeAccountUsage({ account, isZh }: { account: ProviderAccountView; isZh: boolean }) {
  const now = useUsageClock();
  const windows = account.claude_usage?.windows ?? [];
  return <div className="min-w-48 space-y-2 text-xs">
    {!windows.length && <p className="text-slate-500">{isZh ? "用量未知" : "Usage unknown"}</p>}
    {windows.map((window, index) => {
      const valid = Number.isFinite(window.used_percent) && window.used_percent >= 0;
      const label = tierLabel(window.name, isZh);
      const reset = window.resets_at && Number.isFinite(Date.parse(window.resets_at)) ? window.resets_at : null;
      return <div key={`${window.name}:${index}`} className="space-y-1">
        <p className="font-medium text-slate-600">{label}</p>
        {valid ? <>
          <p className={`tabular-nums ${utilizationColor(window.used_percent)}`}>{isZh ? "已用" : "Used"} {window.used_percent.toFixed(1)}%</p>
          <UsageProgress usedPercent={window.used_percent} pacePercent={steadyPacePercent(window, now)} label={`${label} ${isZh ? "已用额度" : "used quota"}`} isZh={isZh} />
        </> : <p className="text-slate-500">{isZh ? "用量未知" : "Usage unknown"}</p>}
        {reset ? <p className="text-[10px] text-slate-500">{isZh ? "重置" : "Resets"}: <time dateTime={reset}>{new Date(reset).toLocaleString()}</time></p>
          : <p className="text-[10px] text-slate-400">{isZh ? "重置时间未知" : "Reset time unknown"}</p>}
      </div>;
    })}
    <p className={account.stale ? "text-amber-700" : "text-slate-400"}>{account.stale ? (isZh ? "旧数据 / 未知" : "Stale / unknown") : (isZh ? "已观察" : "Observed")}</p>
    {account.observed_at != null && Number.isFinite(new Date(account.observed_at).getTime()) && <time className="text-[10px] text-slate-400" dateTime={new Date(account.observed_at).toISOString()}>{new Date(account.observed_at).toLocaleString()}</time>}
  </div>;
}

export function ClaudePoolSummary({ pool, isZh, onRefresh, refreshing = false }: {
  pool: AccountPoolView; isZh: boolean; onRefresh?: () => void; refreshing?: boolean;
}) {
  return <div className={`relative space-y-1 rounded-lg border border-slate-200/70 bg-white/40 px-3 py-2 text-xs text-slate-600 ${onRefresh ? "pr-10" : ""}`}>
    {onRefresh && <PoolRefreshButton onRefresh={onRefresh} refreshing={refreshing} isZh={isZh} />}
    <p>{isZh ? `${pool.counts.total} 个账号 · ${pool.counts.available} 个可用` : `${pool.counts.total} accounts · ${pool.counts.available} available`} · {accountStateLabel(pool.readiness, isZh)}</p>
    <p>{isZh ? `已停用 ${pool.counts.disabled} · 已断开 ${pool.counts.disconnected} · 需要登录 ${pool.counts.auth_required} · 额度耗尽 ${pool.counts.quota_exhausted}` : `Disabled ${pool.counts.disabled} · Disconnected ${pool.counts.disconnected} · Needs login ${pool.counts.auth_required} · Quota exhausted ${pool.counts.quota_exhausted}`}</p>
    <p className="text-slate-500">{isZh ? "各账号的 5 小时、每周和模型窗口独立显示；不同套餐的百分比不可相加或平均。请展开管理账号查看实际用量。" : "5-hour, weekly and model windows are shown per account; percentages across plans are not summed or averaged. Open Manage accounts for actual usage."}</p>
  </div>;
}

function AccountRow({ account, pending, canLogin, usageSummary, onUpdate, onLogin, onRefresh, onRemove, isZh }: {
  account: ProviderAccountView; pending: boolean; canLogin: boolean; isZh: boolean; usageSummary: ReactNode;
  onUpdate: (update: OAuthAccountUpdate) => void; onLogin: () => void; onRefresh: () => void; onRemove: () => void;
}) {
  const [label, setLabel] = useState(account.label);
  const [priority, setPriority] = useState(account.priority);
  const changed = label !== account.label || priority !== account.priority;
  const disconnected = account.status === "disconnected";
  const unsafeLabel = looksLikeCredential(label);
  return <tr className="border-t border-slate-200 align-top">
    <td className="min-w-44 space-y-2 p-2">
      <Input aria-label={isZh ? "账号别名" : "Account label"} value={label} maxLength={120} disabled={pending || disconnected} onChange={(event) => setLabel(event.target.value)} />
      <div className="break-all text-[10px] text-slate-400">{account.account_id} · v{account.status_version}</div>
      <label className="flex items-center gap-2 text-[10px] text-slate-500"><span>{isZh ? "优先级（越小越优先）" : "Priority (lower first)"}</span>
        <Input className="w-20" aria-label={isZh ? "账号优先级" : "Account priority"} type="number" step={1} value={priority} disabled={pending || disconnected} onChange={(event) => setPriority(Number(event.target.value))} />
      </label>
      {unsafeLabel && <p role="alert" className="text-[10px] text-red-600">{isZh ? "别名看起来包含凭证，请改为普通名称" : "This label looks like a credential; use a plain name"}</p>}
      {changed && <Button type="button" variant="secondary" disabled={pending || disconnected || !label.trim() || unsafeLabel || !Number.isInteger(priority) || priority < -2147483648 || priority > 2147483647} onClick={() => onUpdate({ label: label.trim(), priority })}>{isZh ? "保存" : "Save"}</Button>}
    </td>
    <td className="space-y-1 p-2"><p>{accountStateLabel(account.status, isZh)}</p><p className="text-slate-500">{accountStateLabel(account.scheduling, isZh)}</p><p>{account.is_enabled ? (isZh ? "已启用" : "Enabled") : (isZh ? "已停用" : "Disabled")}</p></td>
    <td className="p-2"><ClaudeAccountUsage account={account} isZh={isZh} /></td>
    <td className="min-w-36 p-2"><div className="flex flex-wrap gap-1">
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={onRefresh}>{isZh ? "刷新用量" : "Refresh usage"}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected || !canLogin} onClick={onLogin}>{isZh ? "重新授权" : "Re-authorize"}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={() => onUpdate({ is_enabled: !account.is_enabled })}>{account.is_enabled ? (isZh ? "停用" : "Disable") : (isZh ? "启用" : "Enable")}</Button>
      <Button type="button" variant="secondary" disabled={pending || disconnected} onClick={onRemove}>{isZh ? "移除" : "Remove"}</Button>
    </div>{usageSummary}</td>
  </tr>;
}

export function ClaudeAccountPoolPanel({ provider }: { provider: Provider }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const query = useAccountPool(provider.id);
  const now = useUsageClock();
  const usageStats = useAccountUsageStats(provider.id, "week", now);
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
    <h3 className="text-sm font-semibold">{isZh ? "Claude Code 账号池" : "Claude Code account pool"}</h3>
    <ClaudePoolSummary pool={pool} isZh={isZh} />
    <p className="text-xs text-slate-500">{isZh ? "网关会使用已保存的 refresh token 自动续期。仅在凭证失效时重新授权。" : "The gateway automatically refreshes credentials using stored refresh tokens. Re-authorize when credentials become invalid."}</p>
    {error && <p role="alert" className="text-xs text-red-600">{localizeBackendErrorMessage(error, isZh)}</p>}
    <details open={!pool.config}><summary className="cursor-pointer text-xs font-medium">{isZh ? "账号池调度设置" : "Pool scheduling settings"}</summary>
      <div className="pt-3"><AccountPoolSettings key={JSON.stringify(pool.config)} vendor="claude_code" config={pool.config} pending={pending}
        onSave={(config) => act("configure_provider_account_pool", { config })} /></div>
    </details>
    <div className="flex flex-wrap gap-2">
      {canReauthorizeLegacyProvider(pool, provider.id) && <Button type="button" variant="secondary" disabled={pending} onClick={() => {
        mutation.reset();
        setLogin({ intent: { kind: "reauthorize_legacy_provider", providerId: provider.id, useProxy: provider.use_proxy }, label: "" });
      }}>{isZh ? "原单账号重新授权" : "Re-authorize legacy account"}</Button>}
      <Button type="button" disabled={!pool.config || pending} onClick={() => { mutation.reset(); setLogin({ intent: { kind: "add_account", providerId: provider.id }, label: "" }); }}>{isZh ? "添加账号" : "Add account"}</Button>
      <Button type="button" variant="secondary" disabled={pending} onClick={() => act("refresh_provider_account_pool_usage")}><RefreshCw className={`mr-1 h-3 w-3 ${mutation.isPending ? "animate-spin" : ""}`} />{isZh ? "刷新全池用量" : "Refresh pool usage"}</Button>
    </div>
    {login && <ClaudeAccountLogin key={login.intent.kind === "reauthorize_account" ? `${login.intent.accountId}:${login.intent.expectedVersion}` : `${login.intent.kind}:${login.intent.providerId}`}
      intent={login.intent} initialLabel={login.label} onCancel={() => setLogin(null)} onBound={() => { setLogin(null); void invalidate(); }} />}
    <div className="overflow-x-auto"><table className="w-full text-left text-xs">
      <thead className="text-slate-500"><tr>
        <th className="p-2">{isZh ? "账号 / 版本" : "Account / version"}</th><th className="p-2">{isZh ? "授权 / 调度" : "Authorization / scheduling"}</th>
        <th className="p-2">{isZh ? "实际额度窗口" : "Actual quota windows"}</th><th className="p-2">{isZh ? "操作" : "Actions"}</th>
      </tr></thead>
      <tbody>{pool.accounts.map((account) => {
        const target = { accountId: account.account_id, expectedVersion: account.status_version };
        return <AccountRow key={`${account.account_id}:${account.status_version}`} account={account} isZh={isZh} pending={pending} canLogin={Boolean(pool.config)}
          usageSummary={<AccountUsageSummary stats={usageStats.byAccount.get(account.account_id)} period="week" periodStart={usageStats.periodStart}
            loading={usageStats.query.isPending} error={usageStats.query.isError} locale={locale} />}
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

export function ClaudeAccountPoolCard({ provider }: { provider: Provider }) {
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
    {/* No synthetic pool percentage: plans have different capacities. */}
    {query.data?.config ? <ClaudePoolSummary pool={query.data} isZh={isZh}
      onRefresh={() => refresh.mutate()} refreshing={refresh.isPending} />
    : query.data ? <>
      <ProviderUsageFooter provider={provider} />
      <p className="text-xs text-amber-700">{isZh ? "保存账号池调度设置即可添加更多 Claude 账号；原单账号仍可使用和重新授权。" : "Save pool scheduling settings to add more Claude accounts; the legacy account can still be used and re-authorized."}</p>
    </> : <p className="text-xs text-slate-500">{query.error ? localizeBackendErrorMessage(query.error, isZh) : (isZh ? "读取账号池…" : "Loading account pool…")}</p>}
    {refresh.error && <p role="alert" className="text-xs text-red-600">
      {isZh ? "用量刷新失败" : "Usage refresh failed"}: {localizeBackendErrorMessage(refresh.error, isZh)}
    </p>}
    <Button type="button" variant="secondary" onClick={() => setOpen(!open)} aria-expanded={open}>{open ? (isZh ? "收起账号" : "Hide accounts") : (isZh ? "管理账号" : "Manage accounts")}</Button>
    {open && <ClaudeAccountPoolPanel provider={provider} />}
  </div>;
}
