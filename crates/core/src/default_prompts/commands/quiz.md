---
name: quiz
description: Generate and play an interactive study quiz from a KMS knowledge base, URL, file, or topic — and record the score back into the KMS
whenToUse: When the user runs /quiz <kms|topic|url|file> to build a playable study quiz, or /quiz with no argument to quiz the active knowledge base
---

The user invoked `/quiz` with this argument:

$ARGUMENTS

Build and launch an interactive **study quiz** as a playable widget. The quiz
renders in a built-in interactive widget (no external server, no files written).
YOU generate the questions and the grading data; the widget renders and scores
them locally. When the quiz is drawn from a knowledge base (KMS), the player can
also record the score back into that KMS so progress is tracked over time.
Follow these steps.

## 1. Interpret the argument

Classify `$ARGUMENTS` using this ladder — **stop at the first match**:

1. Starts with `http://` or `https://` → a **URL** to read.
2. Starts with `topic:` → force a **topic** quiz from your own knowledge (strip
   the prefix). Use this to skip the KMS check below.
3. Starts with `kms:` → force a **KMS** quiz (strip the prefix; the rest is the
   KMS name).
4. Names a **KMS** — decide this by *resolution, not guessing*: it either
   appears in the `# Active knowledge bases` section of your system prompt, or
   a probe call `KmsSearch(kms: "<arg>", pattern: ".")` returns **without** the
   error `no KMS named '<arg>' (check /kms list)`. If it resolves → KMS quiz.
   When you auto-resolve a *bare* name to a KMS this way, print one line first:
   `กำลังออกข้อสอบจากคลังความรู้ '<name>' — ถ้าต้องการ quiz หัวข้อทั่วไปแทน พิมพ์ /quiz topic: <name>`
5. Resolves to an existing local path (verify with Read / Glob / Ls) → **file(s)**.
6. Otherwise → a **topic / scope** to quiz on from your own knowledge.
7. **Empty `$ARGUMENTS`**: if one or more KMS are active (listed under
   `# Active knowledge bases`), quiz the **first** active KMS — and if several
   are active, say which one you picked and that `/quiz <name>` targets another.
   If none are active, ask the user what KMS, topic, URL, or file to quiz on.

Also parse any inline options the user included, e.g. "10 questions", "hard",
"mcq only", "เป็นภาษาไทย", "matching". Sensible defaults if unspecified:
~5–8 questions, a MIX of types (mcq, truefalse, short, match), every question
with a short `explanation`. **Write the questions in the same language as the
source material** (e.g. Thai content → Thai questions).

## 2. Gather the content

- **KMS** → gather material **STRICTLY from the KMS — this is closed-book.** Do
  NOT use your own knowledge for question content; a plausible-but-wrong fact
  from memory would corrupt the test of what is actually recorded. Procedure:
  1. Discover the pages: read the KMS index in the `# Active knowledge bases`
     section, or run `KmsSearch(kms, pattern)` with broad keyword stems.
  2. `KmsRead(kms, page)` each relevant page. **Skip** the underscore-prefixed
     `_summary` and `_scores` pages — they are indexes/logs, not source material.
  3. Build every `stem`, `answer`, `choices`, `pairs`, and `explanation` ONLY
     from facts that appear in the pages you read. Each `explanation` should say
     where in the KMS the answer comes from (e.g. "see KMS: <name>/<page>").
  4. If the gathered content is too thin for a fair quiz (fewer than ~3
     substantive facts), DO NOT invent filler. Tell the user the knowledge base
     doesn't have enough material yet and stop. Suggest these **real** ways to
     add content (do not invent commands — there is no `/kms write`):
     - `/research --kms <name> <topic>` — research and save findings into it
       (the `--kms` flag must come BEFORE the query, or /research creates a new
       auto-named KMS instead),
     - `/kms dump <name> <text>` — paste knowledge straight in (the KMS must be
       attached first with `/kms use <name>`),
     - `/kms ingest <name> <file>` — pull in a local file,
     - or just ask in chat (e.g. "เขียนหน้าความรู้เรื่อง … ลง KMS <name>") and I
       will write the page with `KmsWrite`.
- **URL** → use `WebFetch` on it (this needs approval — tell the user you are
  fetching). For very large pages, read enough to cover the material.
- **File(s)** → use `Read` (and `Glob` to expand directories/globs). For large
  files, sample representative sections rather than loading everything.
- **Topic** → use your own knowledge; no fetch needed.

## 3. Write the questions

Produce questions matching this shape (the `QuizRender` tool validates them):

```json
{
  "title": "<short quiz title>",
  "source": "<the KMS name, url, file path, or topic>",
  "kms": "<kms name — ONLY for closed-book KMS quizzes; omit otherwise>",
  "questions": [
    {"type": "mcq", "stem": "...", "choices": ["...", "...", "...", "..."], "answer": 0, "explanation": "..."},
    {"type": "truefalse", "stem": "...", "answer": true, "explanation": "..."},
    {"type": "short", "stem": "...", "answer": "canonical answer", "accept": ["acceptable variant", "another"], "keywords": ["must-contain term"], "explanation": "..."},
    {"type": "match", "stem": "Match each term to its definition", "pairs": [["L1", "R1"], ["L2", "R2"], ["L3", "R3"]], "explanation": "..."}
  ]
}
```

Rules:
- `mcq`: `answer` is the **0-based index** into `choices` (give 3–4 choices).
- `truefalse`: `answer` is a boolean.
- `short`: grading is LOCAL — put lowercase, trimmed acceptable answers in
  `accept` (the canonical `answer` is accepted automatically), and key terms in
  `keywords` (a reply counts as correct if it equals an accepted answer OR
  contains every keyword). Keep `accept`/`keywords` forgiving but not trivial.
- `match`: `pairs` lists the CORRECT `[left, right]` pairings; the widget
  shuffles the right column itself. Use 3–5 pairs.
- Every question needs a self-contained `stem` and a useful `explanation`.

## 4. Render the quiz

Call the **`QuizRender`** tool with the object from step 3. Field rules:
- `title`: a short quiz title (for a KMS quiz, e.g. the KMS name + topic).
- `source`: provenance. For a KMS quiz set it to `"KMS: <name>"`; for a URL /
  file / topic quiz set it to that URL / path / topic.
- `kms`: set this to the KMS name **only** for closed-book KMS quizzes. This is
  what makes the widget show a "save score" button that records the attempt.
  **Omit `kms`** for URL / file / topic quizzes (their scores stay ephemeral).

It mounts a self-contained, playable quiz widget inline in the chat — no files
are written and no external server is used. After it returns, tell the user the
quiz is ready to play (don't ask them to open a URL or a file). For a KMS quiz,
also mention they can press "บันทึกคะแนนลงคลังความรู้" on the result screen to save their score.

## 5. Recording a quiz score into the KMS

When you receive a user message whose **first line is exactly `[QUIZ-RESULT]`**,
it is an automated completion report from the quiz widget — NOT a normal user
message. Do not converse about it; record it. It looks like:

```
[QUIZ-RESULT]
kms: <name>
title: <quiz title>
source: <source>
score: 7/10
percent: 70%
missed:
- <stem of a missed question>
- ...
```

Steps:
1. Parse `kms`, `title`, `source`, `score`, `percent`, and the `missed:` list.
2. Tell the user you are saving (e.g. `กำลังบันทึกคะแนน…`) so the upcoming write
   approval isn't a surprise.
3. If the `_scores` page does not exist yet (a `KmsRead(kms, "_scores")` returns
   a "No such file" error), first create it with `KmsWrite(kms, "_scores", …)`
   using this seeded page, then append the entry:
   ```markdown
   ---
   title: "Quiz scores"
   topic: "Progress log of quiz attempts against this knowledge base"
   type: quiz-scores
   sources: []
   ---

   Auto-maintained log of /quiz attempts. Each entry records date, quiz
   title/source, score, and weak areas.
   ```
   If `_scores` already exists, skip straight to the append.
4. Append a dated entry with `KmsAppend(kms, "_scores", …)` in this exact format
   (use today's date; list each missed stem as a `Weak areas` bullet, or
   `- Weak areas: none 🎉` when nothing was missed):
   ```markdown

   ## <YYYY-MM-DD> — <quiz title>
   - Source: <source>
   - Score: 7/10 (70%)
   - Weak areas:
     - <missed stem 1>
     - <missed stem 2>
   ```
5. Confirm in one short Thai sentence, e.g.
   `บันทึกคะแนน 7/10 (70%) ลงคลังความรู้ '<kms>' แล้ว`.

## 6. CLI fallback (no GUI widget)

If you are running in the terminal / CLI where the widget cannot render, do NOT
call `QuizRender`. Instead run the quiz **conversationally**: present one
question at a time, accept the user's typed answer, grade it with the same rules
as the schema, reveal the `explanation`, then continue. Report the final score
at the end. There is no widget and therefore no "save score" button — so if this
was a **KMS** quiz, record the score yourself: after grading, follow the §5
procedure (you already know the `kms`, title, source, score, and which questions
were missed) to write the entry into the KMS `_scores` page, then confirm.
