// Build URLs for the custom protocol's `/file-asset` handler, which
// serves on-disk files to the WebView (desktop) or hosted webapp. Shared
// by FilesView (file preview) and KmsViewerOverlay (KMS source images).

// Hosted thClaws.cloud serves the webapp under a path/subdomain prefix;
// `window.location.origin` is scheme+host only, so `${origin}/file-asset/…`
// would skip the prefix and 404 at Traefik. Walk the prefix out of
// `location.pathname`. Desktop / single-tenant `--serve` have no prefix.
import { basePath, botQuery } from "../hooks/useIPC";

export function workspacePrefix(): string {
  // Path scheme — thclaws.cloud/u/<handle>/<slug>/… → the 3-segment prefix.
  const u = location.pathname.match(/^(\/u\/[^/]+\/[^/]+)/);
  if (u) return u[1];
  // Subdomain scheme — <handle>.thclaws.cloud/<slug>/… → the slug is the
  // first path segment (handle is in the hostname). The engine still
  // serves at root behind Traefik's strip-prefix, so the file-asset URL
  // just needs this one-segment prefix to route back through Traefik.
  if (/\.thclaws\.cloud$/i.test(location.hostname)) {
    const s = location.pathname.match(/^(\/[^/]+)/);
    if (s) return s[1];
  }
  // Any other mount under a path (an AI Server Suite serves the app at
  // /ui/thclaws/): the page's own path is the prefix, as it is for the WS.
  return basePath().replace(/\/$/, "");
}

// Build a same-origin URL for the file-asset handler. Keeping path
// separators unencoded (segments encoded individually) lets the browser
// treat the URL as a directory structure, so relative references inside
// the served content resolve to sibling files on disk. The backend
// re-validates every request through the sandbox — this is display-only.
export function assetUrl(absPath: string): string {
  const normalized = absPath.replace(/\\/g, "/");
  const segments = normalized.split("/").map(encodeURIComponent).join("/");
  const leadingSlash = segments.startsWith("/") ? "" : "/";
  return `${window.location.origin}${workspacePrefix()}/file-asset${leadingSlash}${segments}${botQuery()}`;
}
