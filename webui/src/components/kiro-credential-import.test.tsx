import { doesNotMatch, match, ok } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { KiroCredentialImport } from "./kiro-credential-import";

const callbacks = { onStart: () => {}, onSubmit: async () => {}, onCancel: () => {} };

test("Kiro import has no browser/callback controls before initialization", () => {
  const html = renderToStaticMarkup(<KiroCredentialImport {...callbacks} isZh={false} pending={false} busy={false} />);
  match(html, /Start import/);
  match(html, /Cancel/);
  doesNotMatch(html, /Open Authorization Page|Copy Link|href=|Callback URL/);
});

test("Both create and re-import use a hidden multiline nonpersistent credential field", () => {
  for (const reimport of [false, true]) {
    const html = renderToStaticMarkup(<KiroCredentialImport {...callbacks} isZh={false} sessionId="fixture" pending busy={false} reimport={reimport} />);
    match(html, /<textarea[^>]+autoComplete="off"/);
    match(html, /spellCheck="false"/);
    match(html, /rows="5"/);
    match(html, /text-transparent selection:text-transparent/);
    match(html, /aria-pressed="false"/);
    match(html, /Show credential/);
    match(html, /never saved in the browser/);
    doesNotMatch(html, /Open Authorization Page|Copy Link|href=|Callback URL/);
    if (reimport) match(html, /replaced only after a successful import/);
  }
});

for (const isZh of [false, true]) {
  for (const reimport of [false, true]) {
    test(`${isZh ? "Chinese" : "English"} ${reimport ? "re-import" : "create"} explains complete single-account exports and preserves legacy formats`, () => {
      for (const pending of [false, true]) {
        const html = renderToStaticMarkup(<KiroCredentialImport {...callbacks} isZh={isZh} sessionId="fixture" pending={pending} busy={false} reimport={reimport} />);
        const description = html.match(/<p class="text-xs text-slate-600">([^<]+)<\/p>/)?.[1];
        ok(description, "Import instructions must be present before and after initialization");
        match(description, isZh ? /kiro-account-lite 的完整单账号导出 JSON/ : /complete single-account JSON export from kiro-account-lite/);
        match(description, isZh ? /无需单独提取 accounts\[0\]\.credentials/ : /no need to extract accounts\[0\]\.credentials/);
        match(description, isZh ? /多账号导出会被拒绝.*每次仅导出并导入一个账号/ : /Multi-account exports are rejected.*export and import one account at a time/);
        match(description, /Kiro IDE \/ CLI/);
        match(description, /refresh token \/ ksk_/);
        match(description, isZh ? /导入 IDE \/ CLI 的 Builder ID \/ IAM Identity Center 凭证时，请同时粘贴客户端注册 JSON/ : /When importing IDE \/ CLI credentials for Builder ID \/ IAM Identity Center, also paste the client-registration JSON/);
        if (!pending) {
          doesNotMatch(html, /<textarea/);
          continue;
        }
        const placeholder = html.match(/<textarea[^>]*placeholder="([^"]+)"/)?.[1];
        ok(placeholder, "Credential field must include paste guidance");
        match(placeholder, isZh ? /kiro-account-lite 完整单账号导出 JSON（不支持多账号）/ : /complete single-account kiro-account-lite JSON export \(no multi-account exports\)/);
        match(placeholder, /Kiro IDE \/ CLI.*kiro-auth-token\.json/);
        match(placeholder, isZh ? /可在后面追加注册 JSON/ : /optionally followed by registration JSON/);
        match(placeholder, /refresh token.*ksk_/);
      }
    });
  }
}

test("Expired or failed imports offer restart instead of accepting stale credentials", () => {
  const html = renderToStaticMarkup(<KiroCredentialImport {...callbacks} isZh={true} sessionId="expired" pending={false} busy={false} reimport />);
  match(html, /开始导入/);
  doesNotMatch(html, /<textarea/);
});
