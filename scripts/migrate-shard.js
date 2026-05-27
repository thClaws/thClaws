#!/usr/bin/env node
/**
 * migrate-shard.js — Migrate flat session files into a 4-level sharded directory layout.
 *
 * Before: .thclaws/sessions/sess-18b1f3dc2e3e8ed8.jsonl
 * After:  .thclaws/sessions/18/b1/f3/dc/sess-18b1f3dc2e3e8ed8.jsonl
 *
 * Usage:
 *   node scripts/migrate-shard.js [sessions_dir]
 *
 * If no directory is given, defaults to ./.thclaws/sessions relative to cwd.
 *
 * Dry-run: add --dry-run to preview moves without executing.
 */

const fs = require("fs");
const path = require("path");

const DRY_RUN = process.argv.includes("--dry-run");
// First non-flag argument after argv[0] (node) and argv[1] (script path)
// is the sessions dir; default to ./.thclaws/sessions.
const SESSIONS_DIR = process.argv.slice(2).find(a => !a.startsWith("--"))
  || path.join(process.cwd(), ".thclaws", "sessions");

/**
 * Compute the sharded subdirectory from a session id.
 * sess-18b1f3dc2e3e8ed8 → 18/b1/f3/dc
 */
function shardPath(sessionId) {
  const prefix = "sess-";
  if (!sessionId.startsWith(prefix)) return null;
  const hex = sessionId.slice(prefix.length);
  if (hex.length < 8) return null;
  return `${hex.slice(0, 2)}/${hex.slice(2, 4)}/${hex.slice(4, 6)}/${hex.slice(6, 8)}`;
}

function main() {
  if (!fs.existsSync(SESSIONS_DIR)) {
    console.error(`sessions directory not found: ${SESSIONS_DIR}`);
    process.exit(1);
  }

  const entries = fs.readdirSync(SESSIONS_DIR, { withFileTypes: true });
  const flatFiles = entries.filter(
    (e) => e.isFile() && e.name.endsWith(".jsonl") && e.name.startsWith("sess-")
  );

  if (flatFiles.length === 0) {
    console.log("No flat session files to migrate. Done.");
    return;
  }

  console.log(`Found ${flatFiles.length} flat session file(s) to migrate.`);
  if (DRY_RUN) console.log("(dry-run mode — no files will be moved)\n");

  let moved = 0;
  let skipped = 0;
  let errors = 0;

  for (const entry of flatFiles) {
    const src = path.join(SESSIONS_DIR, entry.name);
    const shard = shardPath(entry.name);

    if (!shard) {
      console.log(`  SKIP ${entry.name} — id doesn't match sharding pattern`);
      skipped++;
      continue;
    }

    const destDir = path.join(SESSIONS_DIR, shard);
    const dest = path.join(destDir, entry.name);

    // Skip if already at the right place (shouldn't happen for flat files,
    // but guard against double-runs).
    if (src === dest) {
      skipped++;
      continue;
    }

    // Skip if destination already exists (collision).
    if (fs.existsSync(dest)) {
      console.error(`  ERROR ${entry.name} — destination already exists: ${dest}`);
      errors++;
      continue;
    }

    if (DRY_RUN) {
      console.log(`  DRY-RUN ${entry.name} → ${shard}/${entry.name}`);
      moved++;
      continue;
    }

    try {
      fs.mkdirSync(destDir, { recursive: true });
      fs.renameSync(src, dest);
      console.log(`  OK ${entry.name} → ${shard}/${entry.name}`);
      moved++;
    } catch (err) {
      console.error(`  ERROR ${entry.name}: ${err.message}`);
      errors++;
    }
  }

  // Clean up empty 4th-level directories that may be left behind.
  // Walk bottom-up: level 4 → 3 → 2 → 1.
  if (!DRY_RUN) {
    cleanupEmptyDirs(SESSIONS_DIR);
  }

  console.log(`\nDone. moved=${moved} skipped=${skipped} errors=${errors}`);
}

/**
 * Recursively remove empty directories from the bottom up.
 */
function cleanupEmptyDirs(root) {
  function walk(dir) {
    const entries = fs.readdirSync(dir, { withFileTypes: true });
    for (const e of entries) {
      if (e.isDirectory()) {
        walk(path.join(dir, e.name));
      }
    }
    // After processing children, check if this dir is now empty.
    const after = fs.readdirSync(dir);
    if (after.length === 0 && dir !== root) {
      try {
        fs.rmdirSync(dir);
      } catch {
        // ignore
      }
    }
  }
  walk(root);
}

main();
