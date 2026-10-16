import { deepEqual, doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { KiroLoginOptionsForm } from "./kiro-login-options";
import { kiroLoginConfigured, kiroLoginOptions } from "../lib/kiro-login";

const onChange = () => {};

test("Kiro browser sign-in offers all methods and no credential or callback input", () => {
  const html = renderToStaticMarkup(<KiroLoginOptionsForm value={{ method: "google" }} onChange={onChange} isZh={false} />);
  for (const method of ["google", "github", "builder-id", "idc", "import"]) match(html, new RegExp(`value="${method}"`));
  match(html, /value="google" selected=""/);
  match(html, /gateway checks the result automatically/);
  doesNotMatch(html, /<textarea|type="password"|Organization start URL/);
});

test("Identity Center config distinguishes service and authentication regions", () => {
  const html = renderToStaticMarkup(<KiroLoginOptionsForm value={{ method: "idc" }} onChange={onChange} isZh={false} />);
  match(html, /Organization start URL \(required\)/);
  match(html, /Identity Center region \(required\)/);
  match(html, /Model service region/);
  equal(kiroLoginConfigured({ method: "idc", start_url: "https://example.awsapps.com/start" }), false);
  equal(kiroLoginConfigured({ method: "idc", start_url: " ", idc_region: "us-east-1" }), false);
  equal(kiroLoginConfigured({ method: "idc", start_url: "https://example.awsapps.com/start", idc_region: "eu-west-1" }), true);
  deepEqual(kiroLoginOptions({ method: "idc", region: " eu-west-1 ", idc_region: " us-east-1 ", start_url: " https://example.awsapps.com/start " }), {
    method: "idc", region: "eu-west-1", idc_region: "us-east-1", start_url: "https://example.awsapps.com/start",
  });
});

test("Import remains separate and stale configuration does not leak to other methods", () => {
  const html = renderToStaticMarkup(<KiroLoginOptionsForm value={{ method: "import" }} onChange={onChange} isZh={false} />);
  doesNotMatch(html, /Advanced region|<input|gateway checks/);
  deepEqual(kiroLoginOptions({ method: "import", region: "eu-west-1", start_url: "old", idc_region: "old" }), { method: "import" });
  deepEqual(kiroLoginOptions({ method: "google", region: " ", start_url: "old", idc_region: "old" }), { method: "google" });
  deepEqual(kiroLoginOptions({ method: "builder-id", idc_region: "us-east-1", start_url: "old" }), { method: "builder-id", idc_region: "us-east-1" });
  equal(kiroLoginConfigured({ method: "import" }), true);
});
