import DOMPurify from "dompurify";

// DOMPurify's default URI allowlist plus the two in-app schemes rendered
// markdown legitimately carries: `wikilink:` (KMS page links) and
// `thclaws:` (desktop file-asset URLs).
const SAFE_URI =
  /^(?:(?:https?|mailto|tel|wikilink|thclaws):|[^a-z]|[a-z+.-]+(?:[^a-z+.\-:]|$))/i;

/**
 * Make HTML built from untrusted text (model-written pages, fetched web
 * content) safe for `dangerouslySetInnerHTML`. On the desktop the document
 * holds `window.ipc`; on a hosted workspace it shares an origin with the
 * web app's session — a stray `onerror=` reaches either.
 */
export function sanitizeHtml(html: string): string {
  return DOMPurify.sanitize(html, {
    ALLOWED_URI_REGEXP: SAFE_URI,
    FORBID_TAGS: ["style", "form", "iframe", "object", "embed"],
    FORBID_ATTR: ["formaction", "srcset"],
  });
}
