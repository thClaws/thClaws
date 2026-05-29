---
name: quiz
description: Generate and play an interactive study quiz from a URL, file, or topic
whenToUse: When the user runs /quiz <topic|url|file> to build a playable study quiz
---

The user invoked `/quiz` with this argument:

$ARGUMENTS

Build and launch an interactive **study quiz** as a playable widget. The quiz
renders in a built-in interactive widget (no external server, no files written).
YOU generate the questions and the grading data; the widget renders and scores
them locally. Follow these steps.

## 1. Interpret the argument

Classify `$ARGUMENTS`:
- Starts with `http://` or `https://` → a **URL** to read.
- Resolves to an existing local path (verify with Read / Glob / Ls) → **file(s)**.
- Otherwise → a **topic / scope** to quiz on from your own knowledge.

Also parse any inline options the user included, e.g. "10 questions", "hard",
"mcq only", "เป็นภาษาไทย", "matching". Sensible defaults if unspecified:
~5–8 questions, a MIX of types (mcq, truefalse, short, match), every question
with a short `explanation`. **Write the questions in the same language as the
source/topic** (e.g. Thai content → Thai questions). If `$ARGUMENTS` is empty,
ask the user what topic, URL, or file to quiz on, then continue.

## 2. Gather the content

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
  "source": "<the url, file path, or topic>",
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

Call the **`QuizRender`** tool with `{ "title": "<title>", "questions": [ ... ] }`
(the `questions` array from step 3; `source` is optional). It mounts a
self-contained, playable quiz widget inline in the chat — no files are written
and no external server is used. After it returns, tell the user the quiz is
ready to play (don't ask them to open a URL or a file).

## 5. CLI fallback (no GUI widget)

If you are running in the terminal / CLI where the widget cannot render, do NOT
call `QuizRender`. Instead run the quiz **conversationally**: present one
question at a time, accept the user's typed answer, grade it with the same rules
as the schema, reveal the `explanation`, then continue. Report the final score
at the end.
