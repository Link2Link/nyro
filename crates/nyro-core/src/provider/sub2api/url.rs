//! Sub2API address normalization and upstream URL construction.

use super::Sub2ApiPlatform;
use crate::protocol::ids::Protocol;

/// Trailing segments a pasted Sub2API address may carry. One of them (first
/// match, case-insensitive) is stripped to obtain the gateway root; longer
/// suffixes come first so `/antigravity/v1` never leaves `/antigravity`.
pub const ROOT_SUFFIXES: &[&str] = &[
    "/antigravity/v1beta",
    "/antigravity/v1",
    "/antigravity",
    "/backend-api/codex",
    "/v1beta",
    "/v1",
];

/// Strip trailing slashes and one known suffix from a relay address. A
/// reverse-proxy path prefix (`https://edge/sub2api`) survives, and a host
/// literally named like a suffix (`http://v1`) is never treated as a path.
pub fn relay_root<'a>(base_url: &'a str, suffixes: &[&str]) -> &'a str {
    let base = base_url.trim().trim_end_matches('/');
    for suffix in suffixes {
        let Some(split) = base.len().checked_sub(suffix.len()) else {
            continue;
        };
        if !base.is_char_boundary(split) || !base[split..].eq_ignore_ascii_case(suffix) {
            continue;
        }
        let rest = &base[..split];
        // `rest` ending in `/` means the suffix was the host (`http://v1`
        // leaves `http:/`), not a path segment.
        if rest.is_empty() || rest.ends_with('/') {
            continue;
        }
        return rest.trim_end_matches('/');
    }
    base
}

/// Normalize a user-supplied address to the Sub2API gateway root.
pub fn sub2api_root(base_url: &str) -> &str {
    relay_root(base_url, ROOT_SUFFIXES)
}

/// Upstream URL: root + platform route prefix + the encoder's full path
/// (`/v1/...` or `/v1beta/...`, query string included).
pub fn endpoint_url(
    platform: Option<Sub2ApiPlatform>,
    protocol: Protocol,
    base_url: &str,
    path: &str,
) -> String {
    let prefix = platform.map_or("", |platform| platform.route_prefix(protocol));
    format!("{}{prefix}{path}", sub2api_root(base_url))
}

/// Base for helpers that append the standard `/v1/...` / `/v1beta/...`
/// paths themselves (admin probes, connectivity checks): root + prefix.
pub fn route_base(platform: Option<Sub2ApiPlatform>, protocol: Protocol, base_url: &str) -> String {
    endpoint_url(platform, protocol, base_url, "")
}

/// Default model discovery URL for a platform, or `None` without an address.
pub fn models_url(platform: Option<Sub2ApiPlatform>, base_url: &str) -> Option<String> {
    let root = sub2api_root(base_url);
    if root.is_empty() {
        return None;
    }
    let path = platform.map_or("/v1/models", Sub2ApiPlatform::models_path);
    Some(format!("{root}{path}"))
}
