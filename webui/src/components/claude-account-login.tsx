import { useState } from "react";
import { useLocale } from "@/lib/i18n";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { openExternalUrl } from "@/lib/open-external";
import { looksLikeCredential } from "@/lib/kiro-pool-usage";
import { bindClaudeAccountSession, claudeOAuthCompletion, initClaudeAccountSession } from "@/lib/claude-account-login";
import { oauthCompletionMode, useOAuthSession, type OAuthSessionIntent } from "@/lib/use-oauth-session";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { OAuthProgress, OAuthSessionError, OAuthSessionRemaining } from "@/components/oauth-session-status";

export function ClaudeAccountLogin({ intent, initialLabel = "", onBound, onCancel }: {
  intent: OAuthSessionIntent; initialLabel?: string; onBound: () => void; onCancel: () => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const oauth = useOAuthSession();
  const [label, setLabel] = useState(initialLabel);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const legacy = intent.kind === "reauthorize_legacy_provider";
  const unsafeLabel = looksLikeCredential(label);
  const authUrl = oauth.session?.auth_url || oauth.session?.verification_uri_complete || oauth.session?.verification_uri;
  const manual = oauthCompletionMode(oauth.status?.status === "pending" ? oauth.status : oauth.session) === "manual_exchange";

  async function start() {
    if (unsafeLabel) return;
    setError("");
    setCode("");
    const boundLabel = label.trim() || undefined;
    await oauth.start({
      vendor: "claude-code", useProxy: legacy ? intent.useProxy : false, intent,
      init: () => initClaudeAccountSession(intent),
      onReady: async (_status, context) => {
        if (context.intent?.providerId !== intent.providerId) return;
        if (!await bindClaudeAccountSession(context, boundLabel)) return;
        setCode("");
        oauth.reset(false);
        onBound();
      },
    });
  }

  return <section className="space-y-3 rounded-lg border border-sky-200 bg-sky-50/40 p-3">
    <h4 className="text-sm font-semibold">{intent.kind === "add_account" ? (isZh ? "添加独立账号" : "Add a separate account") : (isZh ? "重新授权此账号" : "Re-authorize this account")}</h4>
    <p className="text-xs text-slate-600">{legacy
      ? (isZh ? "保留原单账号授权流程，不会建立账号池或添加第二个账号。" : "Keep the legacy single-account flow; this does not create a pool or add a second account.")
      : intent.kind === "reauthorize_account"
        ? (isZh ? "仅替换所选账号的授权。版本已过期时请关闭并重试。" : "Only the selected account's authorization is replaced. Close and retry if its version has changed.")
        : (isZh ? "追加账号，不覆盖现有授权。" : "Append an account without replacing existing authorizations.")}</p>
    <p className="text-xs text-slate-600">{isZh ? "在浏览器中登录目标 Claude 账号，再粘贴授权码或完整回调地址以交换凭据。不要粘贴 API Key 或访问令牌。" : "Sign in to the intended Claude account in your browser, then paste the authorization code or full callback URL to exchange credentials. Do not paste an API key or access token."}</p>
    {!legacy && <label className="block space-y-1 text-xs"><span>{isZh ? "安全别名（可选，不要填写令牌）" : "Safe label (optional; do not enter a token)"}</span>
      <Input value={label} disabled={Boolean(oauth.session) || oauth.busy} maxLength={120} onChange={(event) => setLabel(event.target.value)} />
    </label>}
    {unsafeLabel && <p role="alert" className="text-xs text-red-600">{isZh ? "别名看起来包含凭证，请改为普通名称" : "This label looks like a credential; use a plain name"}</p>}
    {(!oauth.session || oauth.status?.status === "error") && <Button type="button" disabled={oauth.busy || unsafeLabel} onClick={() => void start()}>{isZh ? "开始授权" : "Start authorization"}</Button>}
    {authUrl && <div className="space-y-2 text-xs">
      <p className="select-all break-all">{authUrl}</p>
      <Button type="button" variant="secondary" onClick={() => void openExternalUrl(authUrl).catch((cause) => setError(localizeBackendErrorMessage(cause, isZh)))}>{isZh ? "打开授权页" : "Open authorization page"}</Button>
    </div>}
    {oauth.status?.status === "pending" && (manual ? <form className="space-y-2" onSubmit={(event) => {
      event.preventDefault();
      if (!code.trim() || oauth.busy) return;
      try {
        const input = claudeOAuthCompletion(code);
        setCode(""); setError("");
        void oauth.complete(input);
      } catch { setError(isZh ? "请粘贴有效的授权码或含 code 参数的完整回调地址。" : "Paste a valid authorization code or full callback URL containing a code parameter."); }
    }}>
      <label className="block space-y-1 text-xs"><span>{isZh ? "Claude 授权码 / 回调地址" : "Claude authorization code / callback URL"}</span>
        <Input type="password" autoComplete="off" spellCheck={false} value={code} disabled={oauth.busy} onChange={(event) => setCode(event.target.value)} />
      </label>
      <Button type="submit" disabled={!code.trim() || oauth.busy}>{isZh ? "交换并绑定" : "Exchange and bind"}</Button>
      <OAuthSessionRemaining expiresAt={oauth.expiresAt} isZh={isZh} />
    </form> : <OAuthProgress isZh={isZh} expiresAt={oauth.expiresAt} retryCount={oauth.retryCount} pending userCode={oauth.status.user_code} />)}
    <OAuthSessionError status={oauth.status} isZh={isZh} />
    {error && <p role="alert" className="text-xs text-red-600">{error}</p>}
    <Button type="button" variant="secondary" onClick={() => { setCode(""); oauth.reset(); onCancel(); }}>{isZh ? "取消" : "Cancel"}</Button>
  </section>;
}
