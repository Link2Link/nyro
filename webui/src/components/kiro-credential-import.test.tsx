import { doesNotMatch, match } from "node:assert/strict";
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

test("Expired or failed imports offer restart instead of accepting stale credentials", () => {
  const html = renderToStaticMarkup(<KiroCredentialImport {...callbacks} isZh={true} sessionId="expired" pending={false} busy={false} reimport />);
  match(html, /开始导入/);
  doesNotMatch(html, /<textarea/);
});
