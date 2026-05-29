---
name: quiz-result
description: Analyze quiz progress over time and the weak areas to study next, from a KMS `_scores` log
whenToUse: When the user runs /quiz-result <kms> (or /quiz-result with no argument) to review how they are doing on a knowledge base and what to revise
---

The user invoked `/quiz-result` with this argument:

$ARGUMENTS

Read the recorded quiz scores for a knowledge base (KMS) and give the user a
clear, honest analysis of their **progress** and the **weak areas they should
study next**. This is read-only — do NOT modify the `_scores` page. Answer in
the same language as the KMS content (Thai content → Thai). Follow these steps.

## 1. Pick the KMS

- If `$ARGUMENTS` names a KMS (it appears in the `# Active knowledge bases`
  section, or a probe `KmsSearch(kms: "<arg>", pattern: ".")` does NOT return
  the error `no KMS named '<arg>' (check /kms list)`), use it.
- If `$ARGUMENTS` is empty and one or more KMS are active, use the **first**
  active KMS; if several are active, say which one you picked and that
  `/quiz-result <name>` targets another.
- If you cannot resolve a KMS, list the available ones (`KmsSearch`/the active
  section) and ask which to analyze, then stop.

## 2. Read the scores log

Call `KmsRead(kms, "_scores")`.

- If it returns a "No such file" error, there are **no recorded attempts yet**.
  Tell the user to take a quiz first with `/quiz <kms>` and press
  "บันทึกคะแนนลงคลังความรู้" on the result screen, then stop. Do not invent data.
- Otherwise, parse the dated entries. Each looks like:
  ```markdown
  ## 2026-05-29 — <quiz title>
  - Source: <source>
  - Score: 7/10 (70%)
  - Weak areas:
    - <missed question stem>
  ```

## 3. Analyze (ground everything in the log — do not fabricate)

- **Progress**: number of attempts and the date span; the score trend over time
  (improving / flat / declining — compare earliest vs latest and note the
  direction); the first, latest, best, and average percentage.
- **Weak areas**: collect the `Weak areas` bullets across ALL attempts and
  group similar ones. Topics that were missed **repeatedly** (in more than one
  attempt) are the highest priority — call these out explicitly. A topic missed
  once and then answered right later is improving, not a weak spot.
- **Map weak areas back to the KMS**: for each priority weak area, run
  `KmsSearch(kms, <keywords from the missed stem>)` to find which KMS page(s)
  cover it, so the user knows exactly what to re-read. Cite as
  `(KMS: <name>/<page>)`.

## 4. Report

Give a concise, scannable summary in this shape (translate the headers to the
KMS language):

- **ความก้าวหน้า** — attempts, trend (เก่งขึ้น/ทรงตัว/ถดถอย), latest vs first %,
  average and best.
- **จุดอ่อนที่ต้องทบทวน** — a prioritized list (most-repeated first); for each,
  one line on what to study and the KMS page(s) to read.
- **ขั้นต่อไป** — concrete next actions, e.g. re-read the cited pages, ask the
  knowledge base about a weak topic in chat, or run `/quiz <kms>` again to
  retest. If a single weak topic dominates, suggest focusing the next quiz on
  it.

Keep it short and actionable — the goal is to tell the learner what to do next,
not to dump the raw log.
