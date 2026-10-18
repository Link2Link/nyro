import { useState } from "react";
import { backend } from "@/lib/backend";
import { useLocale } from "@/lib/i18n";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { canReauthorizeLegacyProvider, type AccountPoolView } from "@/lib/devin-account-pool";
import { openExternalUrl } from "@/lib/open-external";
import { oauthCompletionMode, useOAuthSession, type OAuthSessionIntent } from "@/lib/use-oauth-session";
import type { OAuthSessionInitData } from "@/lib/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { DevinLoginNotice, OAuthProgress, OAuthSessionError, OAuthSessionRemaining } from "@/components/oauth-session-status";

/** Pool sessions have backend-owned targets; unconfigured legacy providers keep the old single-account flow. */
export function DevinAccountLogin({ intent, initialLabel = "", onBound, onCancel }: {
  intent: OAuthSessionIntent;
  initialLabel?: string;
  onBound: () => void;
  onCancel: () => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const oauth = useOAuthSession();
  const [label, setLabel] = useState(initialLabel);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const legacy = intent.kind === "reauthorize_legacy_provider";
  const reauth = intent.kind !== "add_account";
  const authUrl = oauth.session?.auth_url || oauth.session?.verification_uri_complete || oauth.session?.verification_uri;
  const manual = oauthCompletionMode(oauth.status?.status === "pending" ? oauth.status : oauth.session) === "manual_exchange";

  async function start() {
    setError("");
    setCode("");
    // Copy label and intent once; changing controls cannot retarget a ready session.
    const boundLabel = label.trim() || undefined;
    await oauth.start({
      vendor: "devin", useProxy: legacy ? intent.useProxy : false, intent,
      init: async () => {
        if (intent.kind === "reauthorize_legacy_provider") {
          const pool = await backend<AccountPoolView>("get_provider_account_pool", { id: intent.providerId });
          if (!canReauthorizeLegacyProvider(pool, intent.providerId)) throw new Error("ACCOUNT_VERSION_CONFLICT: provider changed; reopen account management");
          return backend<OAuthSessionInitData>("init_oauth_session", { vendor: "devin", useProxy: intent.useProxy });
        }
        return backend<OAuthSessionInitData>("init_provider_account_session", {
          id: intent.providerId,
          ...(intent.kind === "reauthorize_account" ? { accountId: intent.accountId, expectedVersion: intent.expectedVersion } : {}),
        });
      },
      onReady: async (_status, context) => {
        if (!context.isCurrent() || context.intent?.providerId !== intent.providerId) return;
        if (context.intent.kind === "reauthorize_legacy_provider") {
          // Recheck after browser authorization: another tab may have configured
          // or expanded this provider. Core still enforces its atomic ambiguity guard.
          const pool = await backend<AccountPoolView>("get_provider_account_pool", { id: context.intent.providerId }, { signal: context.signal });
          if (!context.isCurrent()) return;
          if (!canReauthorizeLegacyProvider(pool, context.intent.providerId)) throw new Error("ACCOUNT_VERSION_CONFLICT: provider changed; reopen account management");
          await backend("bind_provider_oauth", { id: context.intent.providerId, sessionId: context.sessionId }, { signal: context.signal });
        } else {
          await backend("bind_provider_account", { id: context.intent.providerId, sessionId: context.sessionId, label: boundLabel }, { signal: context.signal });
        }
        if (!context.isCurrent()) return;
        setCode("");
        oauth.reset(false);
        onBound();
      },
    });
  }

  return <section className="space-y-3 rounded-lg border border-sky-200 bg-sky-50/40 p-3">
    <h4 className="text-sm font-semibold">{reauth ? (isZh ? "重新授权此账号" : "Re-authorize this account") : (isZh ? "添加独立账号" : "Add a separate account")}</h4>
    <p className="text-xs text-slate-600">{legacy
      ? (isZh ? "保留原单账号授权流程，无需选择 Pro/Max；不会建立账号池或添加第二个账号。" : "Keep the legacy single-account authorization flow without selecting Pro/Max. This does not create a pool or add a second account.")
      : reauth
      ? (isZh ? "仅替换所选账号的授权，其他账号不受影响。版本已过期时请关闭并重试。" : "Only the selected account's authorization is replaced. Other accounts are unaffected. Close and retry if its version has changed.")
      : (isZh ? "追加账号，不覆盖现有授权。" : "Append an account without replacing existing authorizations.")}</p>
    <DevinLoginNotice isZh={isZh} />
    {!legacy && <label className="block space-y-1 text-xs"><span>{isZh ? "安全别名（可选，不要填写令牌）" : "Safe label (optional; do not enter a token)"}</span>
      <Input value={label} disabled={Boolean(oauth.session) || oauth.busy} maxLength={120} onChange={(event) => setLabel(event.target.value)} />
    </label>}
    {(!oauth.session || oauth.status?.status === "error") && <Button type="button" disabled={oauth.busy} onClick={() => void start()}>{isZh ? "开始授权" : "Start authorization"}</Button>}
    {authUrl && <div className="space-y-2 text-xs">
      <p className="select-all break-all">{authUrl}</p>
      <Button type="button" variant="secondary" onClick={() => void openExternalUrl(authUrl).catch((cause) => setError(localizeBackendErrorMessage(cause, isZh)))}>{isZh ? "打开授权页" : "Open authorization page"}</Button>
    </div>}
    {oauth.status?.status === "pending" && (manual ? <form className="space-y-2" onSubmit={(event) => {
      event.preventDefault();
      const submitted = code.trim();
      if (!submitted || oauth.busy) return;
      setCode("");
      void oauth.complete({ code: submitted });
    }}>
      <label className="block space-y-1 text-xs"><span>{isZh ? "Devin 授权码 / 会话令牌" : "Devin code / session token"}</span>
        <Input type="password" autoComplete="off" spellCheck={false} value={code} disabled={oauth.busy}
          onChange={(event) => setCode(event.target.value)} />
      </label>
      <Button type="submit" disabled={!code.trim() || oauth.busy}>{isZh ? "提交并绑定" : "Submit and bind"}</Button>
      <OAuthSessionRemaining expiresAt={oauth.expiresAt} isZh={isZh} />
    </form> : <OAuthProgress isZh={isZh} expiresAt={oauth.expiresAt} retryCount={oauth.retryCount} pending userCode={oauth.status.user_code} />)}
    <OAuthSessionError status={oauth.status} isZh={isZh} />
    {error && <p role="alert" className="text-xs text-red-600">{error}</p>}
    <Button type="button" variant="secondary" onClick={() => { setCode(""); oauth.reset(); onCancel(); }}>{isZh ? "取消" : "Cancel"}</Button>
  </section>;
}
