// Shared file-asset URL builder. Routes a workspace path through the
// engine's `/file-asset` handler so on-disk files (Files browser
// previews, chat markdown images written by tools like TextToImage)
// resolve to real bytes instead of 404'ing against the page origin.

// In cloud, every workspace is mounted under `/u/<user>/<ws>/` by Traefik
// (see infra/k3s/runner/ingressroute.yaml.j2) and the strip-prefix
// middleware peels that off before forwarding to the runner pod. The pod's
// --serve also registers the file-asset route PREFIXED with the same path
// so the Host()+PathPrefix() rule matches. `window.location.origin` is
// just scheme+host — no path — so `${origin}/file-asset/...` would skip
// the prefix entirely and 404 at Traefik. Walk the prefix out of
// `location.pathname` instead. Desktop / single-tenant `--serve` have no
// prefix; this returns "".
import { basePath, botQuery } from "../hooks/useIPC";

export function workspacePrefix(): string {
  // Path scheme — thclaws.cloud/u/<handle>/<slug>/… → the 3-segment prefix.
  const u = location.pathname.match(/^(\/u\/[^/]+\/[^/]+)/);
  if (u) return u[1];
  // Subdomain scheme — <handle>.thclaws.cloud/<slug>/… → the slug is the
  // first path segment (handle is in the hostname).
  if (/\.thclaws\.cloud$/i.test(location.hostname)) {
    const s = location.pathname.match(/^(\/[^/]+)/);
    if (s) return s[1];
  }
  // Any other mount under a path (an AI Server Suite serves the app at
  // /ui/thclaws/): the page's own path is the prefix, as it is for the WS.
  return basePath().replace(/\/$/, "");
}

// Build a same-origin URL for the custom protocol's file-asset handler.
// Path separators stay unencoded so the browser treats the URL as a
// directory structure (relative refs resolve to sibling files on disk).
export function fileAssetUrl(path: string): string {
  const normalized = path.replace(/\\/g, "/");
  const segments = normalized.split("/").map(encodeURIComponent).join("/");
  const leadingSlash = segments.startsWith("/") ? "" : "/";
  return `${window.location.origin}${workspacePrefix()}/file-asset${leadingSlash}${segments}${botQuery()}`;
}

// Resolve a markdown image `src` for rendering. Absolute URLs (http(s),
// data:, blob:, file:) pass through untouched; a workspace-relative or
// filesystem path (e.g. `output/img-….jpg` emitted by TextToImage) is
// routed through `/file-asset` so it loads the real file. The asset
// handler enforces the workspace sandbox, so this can't escape it.
export function resolveAssetSrc(src: string | undefined): string | undefined {
  if (!src) return src;
  if (/^(https?:|data:|blob:|file:|\/\/)/i.test(src)) return src;
  return fileAssetUrl(src.replace(/^\.\//, ""));
}
