# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.119.0] — 2026-09-04

iApp joins the image providers as the one backend that renders Thai text properly, `/extract` now names where it wrote, and the knowledge sources grow a provenance catalogue.

### Added
- **iApp is available as an image provider — the one that gets Thai text right.** `TextToImage` / `ImageToImage` gain `text` (up to 4 lines) and `font` (19 Thai faces): iApp *typesets* those lines instead of leaving the diffusion model to draw them, which is what stops Thai script coming back as mangled glyphs. Both modes work — a fresh image, or an edit that keeps the original composition. Needs `IAPP_API_KEY`, settable in Settings alongside the other service keys; bring-your-own-key for now.
- **Media Studio offers iApp**, with the Thai text and font fields appearing only when it's the selected model — and the `qwen-image-3.0` pair, which had been missing from the picker since it shipped.
- **`sources/` gets a provenance catalogue and a real ingest path.** Source material is catalogued with where it came from and pulled in through a proper ingest path.

### Changed
- **The model catalogue now prices image generation across the board.** The two Gemini image models had no rows at all; they and iApp are priced from their vendors' published rates, with Appendix A regenerated to match.

### Fixed
- **`/extract` names its output folder in the chat result.** The chat only shows the first line of a background agent's result, so a run that opened with prose left you with no path — the folder is now on that line, ready to open in the Files tab.

## [0.118.0] — 2026-08-29

Teammates launch reliably on Windows, user-pointed providers own their model lists, and the model catalogue gets its pre-release refresh.

### Changed
- **The model catalogue is refreshed ahead of the release.** Pricing and context data are refreshed from the providers, and the online Appendix A is regenerated to match.

### Fixed
- **Teammates now spawn without a shell.** Argv, env and cwd go straight to the process, so there is nothing for `cmd.exe` to mis-quote — `SpawnTeammate` works on Windows without the `THCLAWS_SHELL` workaround. (#200)
- **Team directories on Windows drop the verbatim UNC prefix.** The `\\?\` prefix Rust's `canonicalize()` adds is stripped from the team-dir (and `\\?\UNC\` paths stay UNC), so teammates can actually write their status and inbox files. (#200, #201 — thanks @torandben)
- **User-pointed providers now own their model list.** For a custom endpoint the endpoint, not a fixed list, defines which models are offered.

## [0.117.0] — 2026-08-29

A one-slide-at-a-time PPTX preview lands, the model catalogue gets corrected context windows and pricing, and Qwen-Image 3.x stops rendering smaller images.

### Added
- **PPTX previews now page one slide at a time.** A `.pptx` preview steps through the deck with ←/→ instead of showing every slide in one long scroll; the rendered PDF stays one click away.

### Changed
- **The model catalogue is refreshed with current provider pricing.** Qwen3.8-Flash is priced and the catalogue is refreshed from the provider APIs.

### Fixed
- **Model context windows are sourced from the providers.** The windows for 47 models are pulled from the providers, stale guesses are raised, and the Qwen3.8 and GLM-5.3 values now use their real sizes instead of the provider floor.
- **The Z.ai GLM-5.3 entry no longer inherits GLM-5's price.** The mis-inherited catalogue price is corrected.
- **Qwen-Image 3.x no longer renders smaller images.** The size table that shrank images for free is removed.

## [0.116.0] — 2026-08-25

Qwen-Image 3.0 joins the media models, a co-located AI Server is auto-detected and used as the LLM gateway, and the LTX API key moves into the Settings modal. A round of OpenAI-compat, Windows file-link and Task fan-out fixes lands alongside.

### Added
- **Qwen-Image 3.0 and Qwen-Image 3.0 Pro join the media models.** The two new DashScope image models land in the catalogue alongside LTX 2.3 and LTX 2.5.
- **A co-located AI Server is auto-detected and used as the LLM gateway.** When an AI Server sits alongside thClaws, it is found automatically and the LLM is routed through its gateway.
- **The LTX API key is now settable in the Settings modal.** LTX can be configured from Settings alongside the other media providers.

### Changed
- **The model catalogue is refreshed with current provider pricing.** The four previously unpriced DashScope models now carry prices, and the catalogue is refreshed from the provider APIs.

### Fixed
- **Linked files render on Windows.** Path comparison is now separator-agnostic, so linked files render correctly on Windows.
- **OpenAI-compat context-window handling follows the server and retries.** The local clamp is sized from the server, requests refused as over-context are resized and retried, and thinking is no longer forced off.
- **OpenAI-compat answers are no longer stranded in the thinking channel.** Assembly now delivers them instead of leaving them stuck in the thinking channel.
- **Task calls now fan out concurrently.** The prompt lets the model issue parallel Task calls instead of running them one at a time.

## [0.115.0] — 2026-08-20

LTX 2.5 joins the media models, third-party agent prompts import as native thClaws packages, and LiteLLM becomes a first-class self-hosted provider. A new FolderIndex tool and a round of shell, markdown-preview and OpenAI fixes land alongside.

### Added
- **LTX 2.5 now ships alongside LTX 2.3, selectable everywhere.** The new media model id sits next to 2.3 in the catalogue and is carried through to the web copy and Appendix A.
- **FolderIndex — a deterministic, resumable index of a folder.** The new tool builds a stable index of a folder and resumes an existing index instead of rebuilding it each run.
- **LiteLLM as a first-class self-hosted provider.** A self-hosted LiteLLM endpoint can be selected as a provider like any built-in one.
- **Third-party agent prompts convert into thClaws packages.** The agent importer can turn a third-party agent's prompt into a native thClaws package.
- **Thai capability summary and the full prompt in the import manual.** Agent import now shows the agent's capabilities summarised in Thai and exposes the complete prompt it will run.
- **`/models` asks a user-pointed endpoint and names it when it fails.** The command can be pointed at a user's endpoint, and a failure now names which endpoint failed.

### Changed
- **A workspace pull is narrated instead of going quiet for minutes.** The agent narrates the pull's progress so a long update doesn't read as a hang.
- **A skill's model recommendation survives a retired id.** Recommendations keep working when the model id they point at has been retired.

### Fixed
- **The Shell tab's PTY is told it is a 256-colour terminal.** Colour-probing programs now see a 256-colour terminal and render correctly.
- **Stop now kills the command, not just the process it can see.** Bash's Stop terminates the actual running command rather than only the visible process.
- **Markdown preview keeps `<br>` in table cells, and its links resolve.** Table cells no longer drop `<br>` line breaks, and preview links resolve correctly.
- **The audit desk paints before hydrating, and Stop unwedges the pane.** It renders before loading data, and Stop can recover a stuck pane.
- **The GUI-shell preview no longer hangs.** It answered with the wrong key — so every call hung — and handed the bridge a URL where it wants a path; both are corrected.
- **OpenAI usage is no longer dropped on an unexpected frame shape.** Usage is preserved even when the response frame isn't the shape expected.

## [0.114.0] — 2026-08-11

Every OpenAI model in the picker now actually works: the `gpt-5.6` family accepts tools, the `-pro` tier reaches the endpoint that serves it, and nine models that OpenAI retired are gone for good.

### Fixed
- **`gpt-5.6-luna`, `-sol` and `-terra` refused every request that carried tools.** OpenAI defaults `reasoning_effort` to a non-'none' value for that family and then rejects the combination with function tools, so any agentic turn failed outright with an HTTP error. Nothing in thClaws sets that field — the default is OpenAI's — so the request is now retried once with `reasoning_effort: none`, which is what OpenAI's own error message asks for. The retry keys off that specific refusal rather than a list of model names, so the next model with the same behaviour is handled without a release. These models were unaffected through OpenRouter.
- **The whole `-pro` tier failed on every turn.** `gpt-5-pro`, `gpt-5.2-pro`, `gpt-5.4-pro`, `gpt-5.5-pro` and their dated snapshots are served only by OpenAI's Responses API and answer Chat Completions with "This is not a chat model". They are now routed to the Responses endpoint, where they work.
- **Nine retired OpenAI models are removed, and they stay removed.** The `*-chat-latest` and older `*-codex` ids answer 404 on both endpoints, but OpenAI's model listing still advertises them — so a `make catalogue` refresh kept putting them back after each removal. The refresh now excludes them by name.
- **The OpenAI Responses provider pointed at models that no longer exist.** All three of its catalogue entries were retired ids, including the one it used as its default — selecting the provider without naming a model failed on the first turn. It now carries `gpt-5.3-codex`.

## [0.113.0] — 2026-08-11

46 OpenRouter models that had quietly gone missing are back in the picker, and Meta AI joins as a bring-your-own-key provider.

### Added
- **Meta AI (`api.meta.ai`) is available with your own key.** Set `META_API_KEY`, pick a `meta/muse-spark-*` model, and it works like any other provider — three models, 1M context. Bring-your-own-key only: there is no gateway route, so a hosted workspace cannot reach it without your key.
- **46 OpenRouter models that were missing are now listed**, among them `claude-opus-5`, `claude-sonnet-5`, the `gpt-5.6` family, `grok-4.5`, `glm-5.2`, `qwen3.8-max`, `kimi-k3` and `deepseek-v4-flash-0731`. They were never unavailable — OpenRouter served them all along and typing the id worked — but they did not appear in the picker, so in practice you had to already know the name.

### Fixed
- **The catalogue can no longer fall behind OpenRouter without anyone noticing.** `make catalogue` refreshes every provider from its live API, but OpenRouter was filtered down to a single entry: the refresh could retire dead routes and never add new ones. Adding was a separate command nobody remembered to run, which is how the list drifted 46 models behind. That command is now part of `make catalogue` itself, so a release ships a current list by construction rather than by recollection. Batch-only and variant routes stay out — they are not interactive chat, and 60 of them would have buried the models people actually pick.
- **A truncated API key can no longer be published to the gateway.** The secret-sync step copied whatever `.env` held; a two-character value made it to the live gateway and armed a 401 on every request to that provider. Implausibly short values are now refused by name before anything is written.

## [0.112.0] — 2026-08-11

Context windows across the catalogue are now sourced rather than guessed — including 112 rows that had been advertising eight times the space they actually have — the gateway proxies only the providers it sells, and an MCP server that installs its dependencies on first launch no longer times out doing so.

### Changed
- **The gateway proxies only the ten Featured providers.** `qwen-cloud`, `thaillm` and the Groq **LLM** segment have been withdrawn from the routed set. What the gateway sells and what it proxies had drifted apart — thirteen against ten — which meant a provider outside the sold tier was being proxied, metered and marked up without ever having been offered. **If you use these models on a hosted workspace they will stop working**; on the desktop with your own key nothing changes, since what was withdrawn is the proxy, not the provider. Groq's `/groq/audio` (Whisper) route is unaffected and still billed as before.
- **Model pickers: a guessed context window is shown with a question mark.** Any model whose window the catalogue could not source from the provider's own documentation now renders as e.g. `200k?` rather than passing as a citation.

### Fixed
- **Catalogue: context windows are sourced at scale, and several were badly wrong.** Every remaining guess was checked against the provider's own `/v1/models`, models.dev, and — for DashScope — the published pricing tiers and console model cards. Guesses among the Featured providers fell from 70 to 3, and 340 rows across the whole catalogue gained a real source. The most consequential correction: all 112 `atlascloud` rows claimed a 1M window because that was the provider block's default, while the endpoint serves 131k for Qwen3-235B and 200k for the Claude 4 family. Overstating a window is the direction that breaks requests outright — the engine packs a prompt the endpoint then rejects — so those rows were failing rather than merely compacting early. Two models that no longer exist upstream (`openrouter/openai/gpt-5.3-chat`, `minimax/MiniMax-M2.1-highspeed`) were removed.
- **Catalogue: the writer's context-window guesses are marked instead of hidden.** When the writer filled in a number because no source answered, that value was silently absorbed into the catalogue. It is now tagged as a guess, so the pickers and the appendix can surface the uncertainty instead of a fresh `make catalogue` quietly rebuilding the debt.
- **MCP: the initialize handshake gets its own timeout.** A server launched through `uvx`/`npx` downloads its whole dependency tree before it can answer anything — one such server needs ~38 s on a cold cache and half a second afterwards. The handshake shared the 30-second budget used for tool calls, so that first launch always failed. It now has its own, longer budget (overridable via `THCLAWS_MCP_INIT_TIMEOUT_SECS`), while tool calls keep the short one — a handshake that takes minutes on first run is normal, a tool call that does is a hung server.
- **Cloud sync: archive extraction is bounded in where it writes and how much.** Extraction now refuses entries that would escape the workspace root and stops reading once the decompressed stream passes its size cap, rather than after writing it.

## [0.111.0] — 2026-08-10

Featured-provider context windows are now sourced from real data instead of guesses, high-severity frontend vulnerabilities are patched, and the built-in manual learns to generate its own appendix from the catalogue.

### Fixed
- **Catalogue: featured-provider context windows are sourced rather than guessed.** 120 rows across nine providers were resolved from the provider's own API, LiteLLM, or OpenRouter, correcting 91 values. Rows that resisted every source now carry an explicit "context unverified" marker instead of passing silently as fact, so a floor is no longer mistaken for a specification. (The writer can still fall back to a provider default for newly-added rows; that is tracked separately.)
- **Cloud sync: mistyped flags are refused instead of silently ignored.** A cloud command given an unrecognised flag now errors out rather than proceeding with missing intent.
- **API: artifact snapshot outcomes are machine-readable.** `GET /v1/sessions/{id}/artifacts` used to answer 404 identically whether a run never requested artifacts, requested them and failed, or wrote an unreadable manifest — the failure itself went only to the daemon's stderr. Each outcome now has its own answer, so an orchestrator no longer has to read server logs to find out what happened. [#191](https://github.com/thClaws/thClaws/issues/191)
- **Frontend dependencies: four high-severity advisories patched.** Vulnerable packages in the frontend lockfile were updated to their patched versions.

### Changed
- **Manual: Appendix A is generated from the model catalogue, and now lists context windows.** The hand-maintained version covered 4 of the gateway's 16 providers and had gone two months stale; it is now a view over the catalogue (652 models) carrying each model's context window beside its price. A window nobody published renders as `131k?` rather than as a citation, and a release gate fails if the appendix drifts from the catalogue again.
- **Manual: Chapter 22 is now a tombstone instead of a deletion.** The retired chapter shows a short notice rather than breaking any existing bookmarks or links into it.

## [0.110.0] — 2026-08-09

Thai PII gets masked before it ever reaches the model, the dashboard shows plan expiry on every workspace card, and a stale context-window guess no longer freezes the catalogue.

### Added
- **Sensitive-data masking for Thai PII.** A new engine-wide pipeline detects, tokenizes, and masks Thai personally identifiable information — ID numbers, phone numbers, and other sensitive patterns — before the model ever sees it. A Settings toggle controls masking per workspace; when unmasked values are put back into the output, the model is briefed about what it never saw.
- **Cloud sync numbering and revision tracking.** `/cloud push` and `/cloud pull` now assign incrementing sequence numbers, and `/cloud revision` shows the sync history.
- **Dashboard: plan expiry on workspace cards.** Each workspace card now shows when its subscription or one-shot plan expires.
- **Marketplace: last-30-days skill filter.** The marketplace now supports listing skills published in the last 30 days.

### Fixed
- **Model catalogue: a guessed context window no longer freezes permanently.** A one-time guess from a failed provider probe no longer hard-locks the catalogue entry. [#190](https://github.com/thClaws/thClaws/issues/190)
- **Sensitive-data masking: false positives reduced and arming corrected.** The Thai PII rules were refined for production use and masking now arms at the correct invocation boundaries.
- **File rendering: YAML `---` blocks detected anywhere, not just frontmatter.** Frontmatter-style `---` fences inside arbitrary files are now treated as YAML separators instead of being missed.
- **File rendering: `---` rendered as a horizontal rule, not a setext heading.** A lone `---` line no longer turns the preceding text into an `<h2>`.
- **Billing: lapsed one-shot plans can be renewed.** A plan that expired while still in the one-shot window is now renewable.
- **Plugins: conventional directory layouts counted in contribution stats.** Plugin directories following conventional layouts are now tallied correctly.

### Changed
- **The retired thCompany/Paperclip product line is gone.** The `paperclip-adapter` package source, its technical-manual page, and its public-mirror sync wiring were all removed; references to it were stripped from engine comments and the rest of the manual. The `POST /agent/run` and `GET /v1/agent/info` endpoints it used are unaffected — they stay as generic orchestrator surfaces.

## [0.109.0] — 2026-08-03

Spark Guru lands as a Thai-language DGX assistant that diagnoses and tunes local inference engines, vLLM and llama.cpp get first-class provider support, and the managed browser is now off by default for new workspaces.

### Added
- **Spark Guru: a Thai DGX assistant with benchmarks, model downloader, and an M3 Training Coach.** The new agent diagnoses your local inference setup — a benchmark tab compares six engines (ollama, DeepSeek V4, Gemma, Qwen 9), a model downloader pulls models with hf_transfer for 20× faster downloads, provider switching and an Inference Engine tab let you reconfigure on the fly, deterministic checkup flags misconfigurations, and the M3 Training Coach walks through lab-doctor, lab-coach, and progress tracking.
- **First-class vLLM + llama.cpp providers.** Local inference runtimes get proper provider integration; the unprobed Ollama fallback path is dropped.
- **DGX one-liner installers.** A single command gets a DGX machine up and running with thClaws.

### Changed
- **Managed browser off by default.** New workspaces start with the browser disabled, and `THCLAWS_BROWSER_ENABLED` lets operators turn it off fleet-wide.
- **Gateway default model → DeepSeek V4 Flash.**

### Fixed
- **Sandboxed commands can now access GPUs.** `/dev/nvidia*` and `/dev/dri` are exposed so local inference runs inside the sandbox.
- **Tutorial Studio: tone marks render correctly on desktop.** The tone mark no longer overlaps sara am in Thai text rendering.
- **Local OpenAI-compatible providers: no API key required.** Local runtimes skip the key check instead of demanding a dummy value.
- **Local OpenAI-compatible providers: max_tokens clamped, thinking disabled for tool-calling.** Prevents runaway requests and incompatible parameters in tool-use scenarios.
- **Model catalogue: Qwen3.8-max priced; Gemini Robotics-ER excluded from chat.**

## [0.108.0] — 2026-07-28

Media Studio gets its biggest update yet with speech mode, resumable renders, and a polished UI, plus chat history now preserves thinking and cost footers across every turn.

### Added
- **Media Studio: speech mode, renders that survive a reload, and a full UI pass.** Text → Speech is a first-class mode; a submitted video becomes a background job with live elapsed time that keeps being watched after the shell reloads, instead of blocking the panel; the gallery gained audio cards, type filters, search, and a theme that follows the app.
- **Media Studio: "Enhance" rewrites a prompt on your active model** — translated and filled out for the image model, while text meant to appear *inside* the picture is preserved verbatim (and the rewrite is rejected if it isn't).
- **Media Studio: the rewrite lands in its own box** — your prompt is never overwritten, and Generate uses the enhanced text only while it has any.
- **Chat history: each turn now keeps its thinking block and cost footer** when you reopen a conversation.

### Fixed
- **Media Studio: the viewer's prompt no longer covers the video controls** — it sits below the media, hidden behind a "Show prompt" toggle.
- **Session Explorer: sessions are listed from the store,** not from a prompt that could go stale or time out.
- **Chat: the first turn's thinking block and `[tokens: …]` footer no longer disappear when the turn ends.** A redundant session reload was replaying stored history over the live view.

## [0.107.0] — 2026-07-27

The chatbot gets its biggest settings upgrade yet — Plugins and Connectors panels, My Models with bring-your-own-key, and a raft of chat refinements from pinned conversations to slash commands.

### Added
- **Settings: new Connectors and Plugins panels.** Connectors manages MCP servers — add an HTTP one, see live status and tool counts, remove — and includes the ones installed plugins contribute. Plugins lists installed bundles with what each contributes, and installs from a git or `.zip` URL; skills install from a URL too. The Telegram and Browser-extension placeholders are retired.
- **My Models is now a real provider/model picker with bring-your-own-key** and a sticky search bar.
- **Chat history: pin conversations, bulk-delete, rename, and search.**
- **Chat turns: edit and resend messages, copy code blocks, paste inline images, slash commands, and read-aloud (TTS).**
- **Turn toolbar: an approvals panel, tool-activity feed, stop button, the signed-in account name, per-model usage stats, and voice settings.**
- **`.pptx` files now render as slides** in the file viewer instead of dumping extracted text.

### Fixed
- **Agent SDK `--allowed-tools` / `--disallowed-tools` now gate the MCP bridge,** which was registered after the tool filter ran — same class of bug as the `Task`/`WorkflowRun` fix in v0.106.0.
- **Agent SDK CLI: the bridge-request prompt is answered during init,** cutting ~65 seconds off the first user turn that references an MCP tool.
- **GUI Shell: `fileUrl` resolves relative paths** on desktop instead of returning broken links.
- **Settings panels no longer use body-level dialogs** that the shell sandbox blocks — modals and name-entry fields are now scoped inside each panel.
- **Settings nav icons have a fixed-width column,** so labels don't jump when switching between sections.

## [0.106.0] — 2026-07-27

Images get where they're going — through Telegram, and through the agent SDK.

### Added
- **Telegram: a photo reaches the agent instead of vanishing.** Sending a photo used to do nothing at all — no reply, no log line — because Telegram puts a photo's words in `caption` rather than `text`, so even a captioned photo looked textless and was dropped. Photos now travel the same route the GUI's paste path uses, and an uncaptioned one gets a default prompt. Thanks to [@HelloMAF](https://github.com/HelloMAF) for the diagnosis ([#187](https://github.com/thClaws/thClaws/issues/187)).

### Fixed
- **`agent/*`: a pasted or dragged image reaches the model.** The user turn was serialized as text only, so the image was silently discarded and the model answered as though nothing had been attached ([#185](https://github.com/thClaws/thClaws/issues/185), reported by [@HelloMAF](https://github.com/HelloMAF)).
- **Windows: no more console flash on every turn under `agent/*`.** The `claude` subprocess is spawned once per turn and was missing `CREATE_NO_WINDOW`, so each message popped a console window that stole focus ([#186](https://github.com/thClaws/thClaws/issues/186), reported by [@HelloMAF](https://github.com/HelloMAF)).
- **`--allowed-tools` / `--disallowed-tools` now cover `Task` and `WorkflowRun`.** Both are registered after the filter runs — deliberately, so a spawned subagent inherits the already-filtered tool set — which left the two tools themselves exempt from the operator's own lists.

## [0.105.0] — 2026-07-26

Dependency security pass — vulnerable crates out of the shipped binary and the frontend toolchain.

### Security
- **KMS search index: tantivy 0.22 → 0.26.** tantivy pinned `lru` below 0.16.3, whose `IterMut` violates Stacked Borrows, and `kms_search_index` is compiled into every published binary — so the crate shipped in the artifacts rather than staying a build-time concern.
- **Frontend: 11 of 12 dependency advisories cleared.** js-yaml, postcss, `@babel/core` and brace-expansion move to patched releases inside their existing majors, and the unused `tiptap-markdown` is dropped — it was the only path pulling linkify-it and markdown-it. The remaining advisory reaches brace-expansion through eslint's `minimatch@3`, which has no patched 1.x release; it is dev-only and never reaches the shipped bundle.

## [0.104.0] — 2026-07-26

Plugins survive a workspace move, per-file cloud-sync divergence, and a catalogue refresh.

### Fixed
- **Plugins: a plugin's skills, commands, and agents no longer vanish after the workspace is pushed to a hosted workspace or moved on disk.** The registry recorded an absolute install path, so it stopped resolving anywhere else; it's now stored relative to the registry file itself, and an existing absolute entry is repaired on load.
- **Cloud Sync: divergence is judged per file against each end's own recorded manifest.** A single changed byte on the far end no longer blocks the whole push with `--force` as the only way out — the guard names the files and blocks only on work the other end did.
- **Serve: the "working" indicator comes back when a browser reconnects mid-turn.**

### Changed
- **Catalogue: 2026-07-26 refresh — adds the qwen3.7-flash family, priced from the Alibaba international page.**

## [0.103.0] — 2026-07-26

GUI Shell settings suite, cloud sync reliability improvements, and WYSIWYG editor hardening.

### Added
- **GUI Shell: full settings suite — schedule, heartbeat, skills, knowledge, mode, and profile configuration.**
- **Cloud Sync: push and pull now preserve empty subfolders.**
- **Files: Source ⇄ Rich text toggle when editing a `.md` — edit the raw markdown when byte-exact formatting matters.**

### Fixed
- **Files: WYSIWYG markdown editor destroyed tables, images, HTML comments, `{{…}}` placeholder tokens, and YAML frontmatter on save.**
- **Cloud Sync: divergence detection now bases off the runner manifest instead of local state, preventing false conflicts.**
- **Cloud Sync: content directories named `build`, `dist`, or `target` are no longer silently dropped during sync.**
- **Cloud: bulk sync transfers force HTTP/1.1 to avoid connection failures.**
- **Files: media controls bar no longer bleeds into other tabs.**

## [0.102.0] — 2026-07-23

White-label GUI Shell with sessions and memory bridge APIs, new RenderSlides built-in tool, plugin .zip installs, and catalogue refresh.

### Added
- **GUI Shell: white-label chat shell replaces the chatbot example — markdown, chat bubbles, dark theme, sessions history, and memory panel.**
- **GUI Shell: "Set as default" button on picker cards.**
- **RenderSlides: new built-in tool relays Marp slide decks to the slide-render service.**
- **Plugins: install from a local `.zip` file, not just HTTP URLs.**

### Fixed
- **GUI Shell: settings drawer no longer appears on load and can be closed.**
- **Multiuser: per-user memory root prevents cross-tenant memory leaks.**
- **Chat: autocorrect and spellcheck disabled on the chat input.**
- **RenderSlides: PNG output normalised to `slide-NN.png` (dash separator).**
- **Media: Gemini pro image model corrected to `gemini-3-pro-image` (was returning 404).**

### Changed
- **Catalogue: 2026-07-22 model catalogue refresh.**

## [0.101.0] — 2026-07-21

New built-in TextToSpeech tool, MCP plugin-contributed server fixes, plugin install improvements, and catalogue refresh.

### Added
- **TextToSpeech: new built-in tool powered by Gemini `gemini-3.1-flash-tts-preview`.**
- **REPL: `/tools` prints a deterministic list of all registered tools.**
- **Plugins: install from a local folder path, `--force` reinstall, and self-heal orphaned plugin directories.**

### Fixed
- **Media: TextToSpeech tool now registers correctly in GUI, session, and headless paths.**
- **MCP: `/mcp reauth` resolves plugin-contributed servers, and `/mcp list` includes them on GUI and server.**

### Changed
- **Catalogue: kimi-k3 updated to 1M-context window, AtlasCloud added as a kimi-k3 provider, dead and free-tier models pruned.**

## [0.100.1] — 2026-07-19

KMS project-scoping fix — unqualified create calls default to project scope and the research tool passes scope explicitly.

### Fixed
- **KMS: unqualified create calls now default to project scope, preventing cross-scope duplicate knowledge entries, and the research tool passes `scope:"project"` explicitly.**

## [0.100.0] — 2026-07-17

Windows GUI Shell bridge inline and python3 PATH fix.

### Fixed
- **Windows: the GUI Shell bridge script is inlined so custom tabs load without a fetch round-trip, and a python3 PATH shim is injected at startup so bundled tools resolve correctly.**

## [0.99.0] — 2026-07-17

Tutorial Studio chapter context menus and tab reattach, background-job health reporting, and cloud sync timeout fixes.

### Added
- **Tutorial Studio: right-click a chapter to rename or delete via a context menu.**
- **Tutorial Studio: reopening a closed tab re-attaches to its running background job instead of starting fresh.**
- **Serve: the `/healthz` endpoint reports busy while a detached background job is running.**

### Fixed
- **Tutorial Studio: the job runner strips `--` separators before re-spawning a background job.**
- **Cloud sync: push and pull switch to a no-total-timeout client so long-running transfers don't fail spuriously.**

## [0.98.0] — 2026-07-17

Tutorial Studio batch workflows, Book Author drafting improvements, Moonshot Kimi K3 pricing, and a tier-s memory bump.

### Added
- **Tutorial Studio: a book-to-tutorial flow — per-slide layout picker, GUI buttons for book and prose imports.**
- **Tutorial Studio: batch voice-over picker and a Build-all button for chapter videos.**
- **Tutorial Studio: drafting now gives each chapter whole-course context, and prose files sort by name.**
- **Book Author: the Idea form auto-fills when re-entering an existing book.**
- **Pricing: Moonshot Kimi K3 model ($3 input / $15 output per million tokens).**
- **Runner: tier-s memory limit raised from 1 GiB to 2 GiB.**
- **Cloud sync: push and pull stream through temp files, sync cap raised to 10 GiB.**

### Fixed
- **GUI Shell: an empty asset path now defaults to `index.html` instead of failing. [#183](https://github.com/thClaws/thClaws/issues/183)**
- **UI: streamed slash-command output is folded into a single chat bubble instead of splitting across several.**
- **Cloud sync: `--force-rebind` now implies `--force` on push and pull.**
- **Book Author: fails gracefully when draft staging is missing, and stale staging is cleared on startup.**

## [0.97.0] — 2026-07-15

New BYOK provider, prompt history in Chat, global copy selection, and a GFM table rendering fix.

### Added
- **9router: a self-hosted BYOK provider with an OpenAI-compatible API.**
- **Chat: bash-style prompt history, now shared with Terminal.**
- **Copy: Ctrl/Cmd+C copies a selection everywhere — Chat, Terminal, and agent shells.**
- **GUI Shell: GFM tables now render in the shared chat component.**

### Fixed
- **Doc tools: miscounted GFM table delimiter rows are repaired before rendering.**

## [0.96.0] — 2026-07-15

New BYOK provider, catalogue additions, and fixes for bash cancellation and app layout.

### Added
- **Atlas Cloud: a new BYOK provider with an OpenAI-compatible API, surfaced as a managed key in Settings.**
- **Catalogue: QwenCloud and ThaiLLM model lists are now included.**

### Fixed
- **Bash: a turn cancel now kills the in-flight process, not just its timeout.**
- **UI: the app root is pinned to the viewport top and descendant scroll containers use `overflow-clip`, preventing the navbar from scrolling off-screen and inner scroll from shifting the app viewport.**

## [0.95.0] — 2026-07-15

Research engine improvements, KMS graph toggle, and Files-tab context-menu actions.

### Added
- **KMS graph: a toggle hides orphan (uncited) source nodes from the graph view.**
- **Files tab: "Translate" added to the context menu for `.md` files.**
- **Files tab: "Summarize" added to the context menu for `.md` files.**
- **GUI shell: the chat transcript is now restored when reconnecting to a chat-first shell.**
- **Marketplace: Gmail MCP (community, ISC) added to the catalog.**
- **Subagent: cross-provider model pins are now allowed when the pinned provider is usable.**

### Fixed
- **Research: the ## Sources linker for graph page→source edges is now deterministic.** The linking order previously varied between runs.
- **Research: "auto" depth mode is now actually adaptive.** It honours the max_iter setting (6) instead of ignoring it and falling back to a non-adaptive strategy.
- **Engine: `/cloud push` and `/cloud pull` now hard-error when the working directory is unreadable,** instead of failing silently.
- **Session list: subagent session files are no longer listed as user sessions.**
- **Workflow: the parallel fan-out cap is decoupled from the CPU core count.** It previously capped at the number of cores regardless of the intended concurrency.

### Changed
- **Research: worker roles (fetcher, source-archiver) now run in parallel instead of sequentially.**
- **Research: worker roles are pinned to deepseek-v4-flash for better performance.**

## [0.94.0] — 2026-07-13

Plan-mode and KMS sidebar fixes.

### Fixed
- **Plan mode: the sidebar Approve button no longer disappears the first time you use `/plan` in a fresh workspace.** A startup session reload was resetting the permission mode from Plan back to the default and dropping the approval button before it could be clicked; the reset now only runs on a genuine session switch.
- **KMS: the sidebar knowledge-base list repopulates after returning from a fullscreen GUI-shell tab.** It previously read "None yet" despite an active KMS because the one-shot list snapshot was missed when the sidebar remounted — it now re-requests the list on mount.

## [0.93.0] — 2026-07-12

The tutorial studio gains chapter rename, voice-over generation, a batch generate modal, and a Generate All button, alongside fixes for chapter numbering, slide insertion, slide-type locking, and paste fidelity.

### Added
- **Chapter rename via a hover pencil icon.** Clicking the pencil icon next to a chapter title opens an inline rename field, replacing the previous double-click gesture.
- **Voice-over controls in the slide editor.** Each slide now exposes a voice-over panel with provider and voice selection, plus a generate button for producing spoken audio from the slide contents.
- **Batch 'Generate Slides' modal replaces the split/combine workflow.** The new modal generates multiple slides in one pass and automatically skips slides that already have content, keeping bulk generation fast.
- **Chapter pane 'Generate All' button for regenerating the entire deck.** A single button in the chapter pane regenerates every slide at once, replacing the three individual controls previously required.

### Fixed
- **Chapter number badge is 1-based, matching UI order.** Chapter numbering now starts at 1 in the UI badge instead of 0.
- **+ button inserts a slide after the active slide, not at the end.** Clicking the add-slide button now places the new slide directly after the currently selected slide.
- **Slide type is locked in the AI compose prompt.** The user-selected slide type is now included in the prompt sent to the AI, preventing the model from changing types during generation.
- **Pasted slides carry their rendered assets.** Copying and pasting a slide now preserves all rendered images and assets, not just the text content.

## [0.92.0] — 2026-07-12

GUI shells gain isolated one-shot turns that keep generated output separate from the main conversation, and the tutorial studio ships fixes for slide generation, cloud access, file URL resolution, and video status display.

### Added
- **Isolated one-shot turns for GUI shells.** GUI shells can now request a turn that runs in an isolated context via `streamTurn isolated:true`, keeping generated output separate from the main conversation. The tutorial studio uses this for slide generation (`aiCompose`/`aiPrompt`).

### Fixed
- **Generate Slide persists a slide type change before rendering.** Changing a slide's type and then generating content now saves the type change first, so the generated output matches the new type instead of the old one.
- **Tutorial studio serves as a webapp tab in cloud, not a standalone shell.** Cloud users can now open the tutorial studio as a webapp tab inside the main app, fixing access that previously required a separate shell window.
- **GUI shell `fileUrl` resolves to HTTP in cloud webapp embeds.** File URLs in GUI shells now correctly resolve to HTTP when embedded in a cloud webapp, instead of the non-functional `thclaws://` protocol.
- **Video status pill only appears when a slide has or makes a video.** The video status indicator no longer shows on slides without an existing or in-progress video.

## [0.91.0] — 2026-07-12

`/doctor` checks agent-declared dependencies from the manifest, the tutorial studio ships a redesigned editor with outline alignment, Generate Slide, and modern slide typography, cloud push/pull warns before overwriting diverged work and syncs the full workspace, and engine fixes land for `/reload`, provider error attribution, and stale model sources.

### Added
- **`/doctor` checks agent-declared dependencies from the manifest.** Agents can declare `requires.python` and `requires.system` in their catalog manifest; `/doctor` verifies those dependencies are installed and reports missing ones. The tutorial studio drops its agent-local doctor in favor of this engine-level check.
- **Tutorial studio redesign: outline-aligned editor, Generate Slide, hideable Copilot, and modern slide typography.** The slide editor pane now aligns with the course outline, a Generate Slide action creates slides directly, the Copilot pane can be hidden on wide layouts, and slides render with Kanit + Sarabun fonts for a lighter, airier design. Pillow is preinstalled in cloud workspaces for slide rendering.
- **`/cloud push` and `/cloud pull` warn before overwriting diverged work.** If local and remote workspaces have diverged, push and pull now show a warning before proceeding, preventing accidental data loss.

### Fixed
- **`/reload` restores the live window size, not the last-closed one.** The reload command now captures the current window dimensions instead of restoring whatever size the window had when it was last closed.
- **`/cloud push` and `/cloud pull` sync the full workspace including state and sessions.** Push and pull previously missed the `state/` and `sessions/` directories; they now teleport the complete workspace.
- **Error messages no longer incorrectly blame MCP when a model provider fails to build.** When the current model's provider cannot be constructed, the engine now reports the actual failure instead of attributing it to MCP.
- **Engine no longer reads `~/.claude/settings.json` as a model source.** The user-global Claude settings file is no longer consulted for model configuration, preventing stale or unintended model overrides from leaking in.

## [0.90.0] — 2026-07-11

Admin panel gains per-user usage tracking, per-model cost breakdowns, and credit adjustment, sub-agents persist every run to a child session and skills can declare isolated execution, serve ships a job-runner mode, and cloud workspaces get shell and pre-auth fixes.

### Added
- **Admin panel: per-user usage, per-model cost breakdown, and credit adjustment.** The user management page now shows per-user usage totals, clicking Usage breaks down costs by model, and root users can adjust individual credit balances directly from the management UI.
- **Sub-agent runs persist to child sessions.** Every sub-agent invocation now archives its full transcript to its own child session, so past runs are browsable and accountable instead of evaporating when the sub-agent exits. ([#3](https://github.com/thClaws/thClaws/issues/3))
- **Isolated job-skills run in their own sub-agent.** Skills can declare `isolated: true` in SKILL.md frontmatter; the engine spawns a dedicated sub-agent to run them, keeping long-running jobs from polluting the parent session context.
- **Job-runner mode for `thclaws serve`.** A new `--job-runner` flag resets the session per turn and writes outputs to archive-safe paths, so serve can act as a one-shot job executor without accumulating state across invocations. ([#1](https://github.com/thClaws/thClaws/issues/1))

### Fixed
- **Webapp IPC no longer drops messages during initial WebSocket connect.** Outbound IPC sends during the brief window between page load and WS handshake are now buffered and flushed on connect, preventing silent message loss on real-time views like the PTY Shell tab.
- **Gateway pre-authorisation checks actual balance, not just non-zero.** Requests are now authorised against the available credit balance rather than a simple `>0` check, so users at exactly zero credits are correctly denied instead of slipping through.
- **PTY Shell tab uses bash in cloud workspaces.** Cloud runner shells now default to `/bin/bash` instead of Debian's `/bin/sh` (dash), fixing compatibility with bash-isms in user commands.
- **Cloud workspace seeds no longer pin the default model.** Gateway-provisioned workspaces no longer bake a pinned model into `settings.json`, so users see their actual default model instead of a stale seed value.

## [0.89.0] — 2026-07-10

Tutorial Studio gets a NOTE-first /outline skill and deterministic slide rendering, the Business Agent ships as a new first-party offering, KMS ingest grows quality-of-life improvements, and search gains a real Google backend via SerpAPI alongside fine-grained tool-approval controls and a tool-loop breaker.

### Added
- **Tutorial Studio: /outline skill, structured outline import, and deterministic slide rendering.** A new `/outline` skill supports NOTE-first authoring directly in the workspace, `outline-import` ingests structured outline markdown files incrementally by slide ID, and the slide-layout renderer is now deterministic with consistent spacing in video builds.
- **Business Agent — a new first-party agent with per-task GUI.** Ships vendored Anthropic knowledge-work packs alongside a GUI shell that exposes a dedicated interface per task type.
- **Force-approval on specific tools under `permissions:auto`.** The `askTools` setting lets you mark individual tools as always-require-approval even when the session-wide policy is auto-approve, so dangerous tools stay gated without switching the whole session to manual.
- **KMS ingest now auto-authors a curated page, copies local images, and handles alias collisions.** Ingesting a source auto-generates a curated landing page (replacing the stub), copies local images referenced in markdown, and offers a Replace action when an alias collides. An "Add to KMS" context action on `.md` files in the file tree lets you ingest without leaving the workspace.
- **Degenerate-tool-loop breaker.** After three consecutive rounds where every tool call fails, the engine now breaks the loop instead of burning tokens indefinitely.
- **Descriptive tool-call progress in the side channel.** Slash commands like `/extract` now stream human-readable tool-call labels instead of raw function names.
- **SerpAPI search backend with Settings key entry.** Enter a SerpAPI key in the Settings modal to get real Google search results through the search tool — full engine, gateway, and docs integration.

### Fixed
- **Tutorial Studio: Thai text shaping.** Slide rendering now uses HarfBuzz (libraqm) for correct Thai glyph placement when available.
- **Unpinned subagent workers now track mid-session model switches.** Subagents launched without a pinned model follow your active model selection when you switch models mid-session instead of sticking with the original.
- **Agent publishing: dry-run lies, republish pair loss, and missing settings.json files.** `publish --dry-run` no longer claims a private agent doesn't exist when it does, republishing a private agent restores the public/private pair correctly, and settings.json files for training-video-generator, webapp-coder-workflow, and tutorial-studio that were silently dropped from the catalog are restored and tracked.
- **Research agent: deterministic source links and args resilience.** `../sources/` links are now derived deterministically from the URL slug instead of varying across runs, and a missing `args.query` throws a real error (instead of silently passing `undefined`) with an `args_json` hint for models that stringify args.
- **Thinking bubble no longer crashes the webview.** Unbounded reasoning streams are capped so the GUI doesn't crash on extremely long thinking blocks.
- **Grok-4.5 WorkflowRun args preserved.** Schema property ordering is now preserved so Grok-4.5, which drops unordered args, correctly passes structured input to WorkflowRun.
- **KMS viewer renders source images.** Source images embedded in KMS pages were 404 in the viewer — they now resolve and display correctly.
- **Content Extractor prefers WebFetch and stops saving placeholder sources.** The extractor now uses WebFetch (with HAL) as its primary fetch path and no longer writes a placeholder source file before extraction completes.
- **Gateway-active workspaces force `halEnabled` on.** HAL-dependent features like browser-rendered WebFetch no longer silently break in gateway-provisioned workspaces.

## [0.88.0] — 2026-07-09

Tutorial Studio ships as a first-party agent for building AI-native courses end-to-end, the engine gets a working hooks system alongside job artifacts and a full headless surface, and the research / book-author agents level up with cross-linked KMS pages, source caching, and optional chapter targets.

### Added
- **Tutorial Studio — a new first-party agent for building courses.** Tutorial Studio brings the full movie-maker pipeline into a GUI shell: title-addressed slide editing, AI-powered prose-to-slide drafting, provider-level TTS (Gemini / OpenAI / MiniMax / ElevenLabs) with per-slide voice selection, image generation with user-editable style rules and multi-reference images, batch image+voice gen across a deck, AI-generated slide video from Grok / Seedance / LTX / Veo, and a configurable `output.json` for media defaults. Async submit/poll keeps long video jobs out of Bash timeouts, and a generic background job runner handles the rest.
- **Job Artifacts — Bearer-authenticated, session-scoped file transfer.** `JobArtifactUpload` and its read counterpart let agents pass tamper-proof files between workflow steps without leaving them in the workspace. Each artifact gets a signed token; the reader verifies the signature before accepting the payload.
- **`-p` is now a full headless surface.** The `thclaws -p "prompt"` one-shot learned session heartbeats and `--resume-session`, so scheduled / cron-driven prompts share a single session and message history instead of starting cold every time.
- **Research agent: cross-linked KMS pages with source caching.** Research splits findings into multiple cross-linked KMS pages instead of one monolithic output, caches fetched sources offline into `sources/` for later inspection (via the new `KmsWriteSource` tool), and surfaces a **Depth** control in the quick-actions row so you dial 2–6 research rounds for simple topics.
- **Book Author: KMS ingest, export folder, and markdown rendering.** The book-author can now read a research agent's KMS as sources (`import-kms`, auto-run from `/sources-ingest`), export books as a self-contained folder (`--format folder`) for direct consumption by Tutorial Studio, render chapter markdown with full GFM support (tables, code blocks, lists), and skip `--target-words` / `--target-chapters` — the outline sizes small books itself.
- **Book Author: per-chapter regen with live progress.** Each chapter gets a **Regen** button that re-drafts just that chapter using the currently selected model. Parallel drafting shows per-chapter live progress, and figures now appear inline at their reference point instead of being dumped at the chapter end.
- **`KmsCreate` auto-attaches the new KMS to the active set.** Creating a KMS project now attaches it automatically so you can start writing immediately without the extra attach step.
- **WorkflowRun: retry with error feedback and live progress streaming.** Subagent retries now feed the rejection reason back into the next attempt so the model can self-correct. WorkflowRun progress streams to the chat mid-run instead of appearing only when complete, and stderr lifecycle breadcrumbs make failures debuggable.
- **`thc-split` — draggable pane splitters in shell UIs.** The engine ships a shared UI runtime component so any GUI shell can add resizable pane separators with a single custom element.
- **HTTP Range support on file assets.** The built-in file server now supports range requests so video files stream with proper seeking instead of stuttering through buffered downloads.
- **Agent Builder v0.2.0.** Wraps the native scaffolder, audits agents against the §0.5 / §4.6 meta-rules, and adds an optional GUI shell build phase.

### Changed
- **Book Author subagents are model-unpinned.** The outliner, fact-checker, and researcher subagents no longer force a specific model — they follow your active model selection, so you can draft with any provider.
- **Research, Book Author, and Tutorial Studio share one KMS root.** All three agents now use `.thclaws/state/kms` as the canonical KMS path, so findings pass between agents without path mismatches.

### Fixed
- **Hooks in `settings.json` never fired.** The config deserializer was silently dropping the `hooks` block — no hook (before/after turns, file watchers, shell triggers) has ever run. They now deserialize and execute.
- **`KmsWriteSource` was missing from 8 of 12 tool-registry sites.** The tool that lets research cache fetched sources now appears in every context where agents might call it.
- **Book Author: bold-wrapped citation refs and extension mismatches.** Export now strips the padded bold from `**{cf:...}**` patterns so citations render clean, and chapter figure links no longer 404 when the file is `.png` but the link says `.jpg`.
- **Research agent: planner resilience and multi-page fix.** The planner now retries on empty angle lists instead of ending the run, accepts bare JSON arrays from models that skip the `{subtopics}` wrapper (Qwen / GLM), and no longer collapses multi-page fetches or skips source caching. One excerpt-less source in a batch no longer fails the entire fetch.
- **Research agent: budget and citation hygiene.** Output budgets scale properly for book-sized topics, uncited sources are trimmed from the final list, and KMS pages are actually persisted — a missing `KmsWrite` call meant prior runs wrote nothing.
- **WorkflowRun: JSON-string args and gateway env for Bash children.** Args passed as a JSON string (as Qwen / GLM do) are now tolerated, and the engine re-injects resolved gateway environment into single-user Bash children so provider credentials reach tools launched from workflows.
- **Content Extractor regression.** Articles save as `articles/<slug>/<slug>.md` (not `article.md`), restoring the per-article directory layout.

## [0.87.0] — 2026-07-05

Movie Maker polish and a cleaner model list, on top of a full pass to make the manuals match the code.

### Added
- **Movie Maker: a running budget ceiling.** `FilmGenerate` already refused to start a film that costs more than the `budgetUsd` you confirmed; now it also stops *mid-render* before the next shot once actual spend passes that budget — so per-shot overruns can't quietly blow past your cap. Already-rendered shots are kept; raise the budget and resume to continue.

### Changed
- **`codex/` models are hidden from the model picker.** The `codex/` (OpenAI Responses) models don't work through the hosted gateway and overlap the plain `gpt-5-codex` rows, so they no longer clutter the picker. Routing still works if you type `/model codex/<id>` explicitly.
- **Documentation refresh.** The user manual (EN + TH) and the technical manual were audited against the current code and updated end-to-end — the provider list (now 25 providers; Agentic Press removed), the GUI-shell bridge, gateway metering, cloud visibility (public/unlisted/private), and the built-in tools. New chapters cover **Movie Maker / FilmScript**.

### Fixed
- **Movie Maker TTS is billed, not bypassed.** Gemini / OpenAI / MiniMax text-to-speech in the film pipeline now route through the metered gateway (or fail closed) instead of calling the provider directly, so hosted narration is attributed and billed correctly.
- **`WatchVideo` and `FilmAssetImport` now ask before running.** Watching a clip runs a paid transcription, and asset import writes up to 30 MB — both now prompt for approval like other spend/side-effecting tools.
- **Generated media is per-user in shared workspaces.** Images/videos land under your own workspace folder instead of a shared `output/`.

## [0.86.0] — 2026-07-05

Custom GUI shells graduate to first-class apps: they can approve tool calls in their own UI, stream a turn event-by-event, upload files, and store data — and the catalog now shows exactly what a shell can do before you install it. Plus a fix for `agent/*` models failing when thClaws is opened from the Dock.

### Added
- **GUI Shell inline tool approvals.** A shell with its own approve/deny UI no longer gets the full-screen system prompt on a mutating tool call — it renders its own widget (`thclaws.approvals.subscribe`/`respond`) and the engine waits for that verdict instead.
- **GUI Shell `streamTurn` — event-by-event turns.** `thclaws.streamTurn(prompt)` returns an async-iterable that yields text, tool calls, and tool results in arrival order, for turn-by-turn shell UIs.
- **GUI Shell `uploadFile`.** Shells can upload a file into the workspace and get back a servable URL to use as an `<img>`/link — per-user isolated in shared workspaces.
- **GUI Shell storage: delete + quota.** `thclaws.storage.delete(key)` removes a key (distinct from setting it null); per-shell storage is now capped at 10 MB.
- **Catalog shows shell capabilities before install.** An agent's detail page now lists what its shell can do ("Run the Bash tool", "Read your knowledge base", …) so you can see what you're consenting to.

### Fixed
- **`agent/*` models now work when thClaws is launched from the Dock/Finder or a scheduled job.** These models run the Claude Code `claude` CLI as a subprocess; a GUI/scheduled launch inherits a minimal `PATH` and couldn't find it (`spawn claude: No such file or directory`). thClaws now locates `claude` in the usual install spots even when `PATH` is bare, and gives an actionable error if it's genuinely missing.
- **GUI Shell bridge calls no longer hang.** Several bridge methods (`storage.delete`, `permissions.list`/`has`, `awaitApproval`, `uploadFile`) had no engine handler and left their promise pending forever; they're now wired (or fail fast with a clear error), and every bridge call self-times-out rather than hanging.
- **Custom shells can only invoke the tools they declare.** A shell's tool calls are now gated by its manifest `tools.invoke:*` permissions instead of being unrestricted.

## [0.85.0] — 2026-07-03

FilmScript: turn a screenplay into a finished AI short. A new `.film` DSL and the Movie Maker agent compile a script into a multi-backend video with Thai dialogue, and the agent can now watch its own output back. Plus scheduled-run error visibility.

### Added
- **FilmScript (`.film`) DSL + Movie Maker agent.** Write a screenplay, compile it, and generate a finished short — character/scene consistency, dialogue, music, and subtitles. A pure two-phase compiler produces the backend payload; a harness runs TTS + generation + assembly. You author; the tools run the deterministic pipeline, showing a cost estimate before anything is spent.
- **Multi-backend video generation.** Grok Imagine by default (best Thai dialogue + scene consistency), with LTX-2, Google Veo, Seedance, and Alibaba Happy Horse selectable per film or per shot via `backend:` / `@backend:`. Each backend's capabilities (image refs, continuation, audio, resolution) are validated at compile time.
- **`WatchVideo` tool — the model can watch a video.** Extracts scene-aware, deduplicated key frames from a local video and returns them as inline images so a vision model actually *sees* the footage, plus an optional Whisper transcript (via `GROQ_API_KEY`). Use it to review or critique a clip or answer questions about what happens on screen.
- **Subtitles rendered onto the film.** Every spoken line becomes a caption: soft-muxed as a toggle-able track by default, or hardcoded into the picture with `subtitle_burn: on` (styled Thai captions for social platforms).
- **Multi-provider TTS.** ElevenLabs (`eleven_v3`, the default Thai voice) and OpenAI (`gpt-4o-mini-tts`) join Gemini and MiniMax; each voice picks its provider and model in `voices.json`.
- **Dialogue voicing modes.** `dialogue_sync: native` (default — the backend speaks the line), `overlay` (swap in an exact TTS voice), or `lipsync` (re-sync the mouth to the TTS voice).
- **Movie Maker genre playbooks.** Loadable skill references for drama (dialogue coverage — shot/reverse-shot, eyelines, shot-size by beat) and documentary (voice-over master clock, silent b-roll, subject lock).

### Fixed
- **Scheduled runs: see *why* a fire failed.** `schedule status` now prints the diagnostic log path under any errored fire, so a bare `err` is no longer a dead end — the actual error is one `cat` away.

## [0.84.0] — 2026-07-01

Major UI and cloud-dashboard refreshes: dedicated Access Keys page, refined workspace visuals, and a surge of new features for custom GUI shells.

### Added
- **Dedicated "thClaws.cloud Access Keys" page.** Added a full-page management view for access keys at `/access-keys`, accessible independently of the dashboard.
- **GUI Shell: standard theme and header.** Introduced a standard theme, `<thc-header>` chrome, sidebar layout with a pinned model picker, and full-screen toggles for GUI Shell extensions.
- **GUI Shell: deterministic research data APIs.** Added deterministic `thclaws.kms/research` data APIs for GUI Shells, enabling non-LLM-backed access to research data.
- **GUI Shell: shared conversation component.** Provided a standard `<thc-chat>` conversation block, including contextual grey tool chips, for all GUI Shells.
- **GUI Shell: searchable model picker.** Added a main-app-style, searchable model picker component for custom shells.
- **GUI Shell: permission-gated model picker.** Enabled permission-scoped `<thc-model>` provider + model picker for shell environments.
- **Cloud: live workspace push/pull streaming.** `/cloud` sync commands now stream transfer progress live with upload percentages.

### Fixed
- **Access Keys link and menu.** Updated both the Access Keys menu and credit link to point to `/access-keys`, fixing previous routing issues.
- **Access Keys page discoverability.** Moved Access Keys to its new location, ensuring the page is reachable from the dashboard.
- **Workspace-list visuals.** Workspace list icon now uses a 16:9 uncropped image, and agent entries show a small icon instead of a cropped banner.
- **Sidebar model selection.** Prevents flashing a stale model when exiting full-screen mode.
- **Research Agent.** Improved adaptive depth, page-scoped gates, and final re-verification logic in the research-agent.

### Changed
- **Cloud: removed gateway-key API.** Deprecated and removed the user-facing gateway-key API library, REST endpoint, and related router hooks.

## [0.83.0] — 2026-06-30

Auto-discovers OpenCode-Go model routing from the live model list, so new models reach the correct API dialect without a code change.

### Added
- **Auto-discovered wire routing for OpenCode-Go.** The OpenCode-Go provider now probes `/v1/models` on first use and caches each model's `wire` hint, so newly added models route to the correct API dialect (OpenAI / Anthropic / Alibaba) automatically — no static-list edit or release required. Probe failures, or models without a hint, fall back silently to the built-in tables, preserving offline and backward-compatible behavior. (#175)

## [0.82.0] — 2026-06-30

Consolidates all KMS stores with a new `/kms consolidate` command and documents cloud workspace sync in the tutorial capstone.

### Added
- **KMS consolidate command.** `/kms consolidate` now folds every writable knowledge base into a single unified KMS for streamlined organization.
- **Cloud Workspace Sync tutorial.** Adds Chapter 23 to the tutorial covering `/cloud push|pull` workflows, completing the Capstone with up-to-date cloud sync guidance.

## [0.81.0] — 2026-06-30

Introduces seamless desktop–cloud workspace sync with resilient push/pull flows, and polishes cloud onboarding with broader user auto-grants.

### Added
- **Desktop-to-cloud workspace sync.** `/cloud push|pull <slug>` commands and Settings panel now enable direct and incremental sync between local and hosted workspaces, with robust em-dash and Unicode slug support.
- **Push/Pull UI in Settings.** Adds one-click Push/Pull buttons to the desktop app for immediate workspace transfers to and from thClaws.cloud.
- **Auto-grant for cloud guests.** Whitelisted cloud guests are now automatically granted user status on every sign-in for smoother onboarding.

### Changed
- **Incremental sync and resume.** Push/pull operations now use a manifest-diff with built-in auto-resume for interrupted transfers.

### Fixed
- **Sync endpoint reliability on single-tenant runners.** Repairs 404 errors when syncing workspaces in single-tenant cloud runner environments, resuming stalled probes.

## [0.80.0] — 2026-06-29

Strengthens PDF and KMS ingest for Thai documents, advances agent publishing via source-level visibility and batch tooling, and brings tutorial documentation current with capstone and GUI coverage.

### Added
- **Vision-based PDF ingestion in /kms.** `/kms ingest <name> <file.pdf> --vision` enables robust extraction of content from scanned and image-heavy PDFs.
- **Agent publishing batch/changelog flow.** `publish.py --changed` updates only modified agents, streamlining batch publishes with tighter log output.
- **Built-in image tool integration for agent image-generator.** Agents can now leverage native image tools for generation tasks.

### Changed
- **Private-by-default visibility and owner re-publish in cloud API.** Published sources default to private status; owners can explicitly re-publish.
- **Credential-aware default model selection + always-on gateway.** The engine picks a sensible default model based on credentials and enables the gateway by default.
- **Source-level platform publish identity in cloud API.** The publishing flow now uses source-level identity for audit trails and clarity.

### Fixed
- **Thai PDF ingest quality and normalization.** Improves text extraction fidelity for Thai PDFs, repairing sara-am clusters and normalizing text reuse; vision param properly switches to OCR when needed.
- **GUI shell and capstone documentation.** Updates tutorial slides with screenshots for chapter 20 (capstone), ch17 (LINE/Telegram), ch18 (GUI shells), and the Folder instructions editor.
- **Agent listing robustness.** Batch slugging and contact-sheet handling for agents now safely accommodate Thai and non-Latin characters.
- **Artifact cleanup on cloud publish.** Ensures runtime artifacts are stripped during cloud publish, matching Rust and Python code paths.
- **Installer settings preservation.** Maintains local installer settings when calling `/cloud get`.
- **Subagent pin fallback.** Ensures fallback to the session model when cross-provider pins are unavailable.

## [0.79.0] - 2026-06-28

Rounds out guest gating for the cloud, hardens the approval box UI with multi-line support and path redaction, and closes the capture-to-retrieve loop in the knowledge base for a self-maintaining /kms.

### Added
- **Self-maintaining knowledge base.** The KMS capture→retrieve loop is now closed with `/kms maintain` and linked auto-retrieval, aligning with the dream redesign.
- **Multi-line approval box and path redaction.** The Key: value approval prompt supports multi-line values and redacts sensitive paths when presenting actions to the user.
- **All-chapters deck export.** The tutorial deck builder adds `md-to-pptx --combine` for an all-chapters PowerPoint export.

### Changed
- **Guest gating in the cloud.** Non-whitelisted or unauthenticated users are now admitted as gated ‘guest’ users in the cloud, restricting CLI token access, purchases, and exposing a gated coupon redeem flow. The guest state is made explicit in the UI.

### Fixed
- **Tutorial screenshots.** Updates tutorial documentation with real screenshots for the approval box and memory flow sequence.
- **HAL tool registration in tests.** HAL tools are now registered explicitly in visibility tests to ensure test coverage consistency.

## [0.78.0] - 2026-06-27

Adds a HAL tools opt-in flag, fixes HAL availability over the gateway and the /model picker, hardens OpenAI-compatible tool-call pairing, and continues tutorial screenshot wiring.

### Added
- **HAL tools opt-in.** New Settings → Optional features toggle enables the HAL tools (YouTubeTranscript, WebScrape); off by default, registered per surface like the media tools.
- **Tutorial screenshot updates.** Wires ch04b/c/d screenshot sequences (s03–s06), aligns ch04c HAL narration, and splits ch04b slide 5→5+5b.

### Changed
- **/model picker for two-model providers.** The picker now opens for providers with exactly two models (e.g. DeepSeek), not only three or more.

### Fixed
- **HAL tools available over the gateway.** HAL tools now appear when the gateway is active (desktop proxy or cloud pod) even with no local HAL_API_KEY — availability uses the same config-aware signal as gateway routing.
- **OpenAI-compatible tool-call pairing.** Dedups duplicate tool_call ids and keeps each turn's tool results adjacent to their assistant call, fixing intermittent "insufficient tool messages following tool_calls" errors from strict endpoints (DeepSeek, …).
- **HAL row icon.** Uses the FileText icon; the Youtube icon isn't exported by this lucide-react version, which broke the frontend build.

## [0.77.0] - 2026-06-26

Adds auto-image resize for vision, gateway-side payload bounding, monorepo orientation docs, and stability+behavior fixes across agent, team, and serve-server. Several agent tutorial chapters expanded.

### Added
- **Auto-image downscaling for vision use.** Oversized images passed to vision models are now automatically downscaled to fit under the 5MB payload limit instead of erroring.
- **Gateway image payload bounding.** Outgoing image payloads are now capped at the gateway 5MB body constraint, enforcing the limit before upload.
- **Root CLAUDE.md with monorepo orientation and deploy flow.** Adds a centralized guide for contributors and internal releases.
- **Toggle-finder-hidden utility.** Exposes a quick util to toggle visibility of hidden files in Finder.
- **User tutorial expansion.** New/expanded agent team/collaboration chapters (ch04b/c/d, ch16), updated screenshots in ch13/14/15, and a markdown-to-PPTX generator.

### Changed
- **PdfRead vision fallback for Thai text.** Garbled Thai text from PDFs now routes to the vision-OCR fallback before extracting content, further increasing extraction fidelity.

### Fixed
- **OpenAI error labeling.** Prevents mislabeling a size-capped 4xx error as "model not vision-capable" when the issue is actually request size.
- **Silence confine dead-code warning on non-Linux.** Suppresses noisy compiler warnings regarding unused code in confine_runtime_failed.
- **Confine fallback logic.** Falls back to unconfined mode cleanly if the OS confiner can't enforce sandbox boundaries.
- **Serve tool-approval prompt delivery.** Tool approval prompts now deliver correctly over WebSockets, unblocking the turn for multiuser or hosted net.
- **Serve multiuser safety net + override.** Ensures auto-approval always applies in hosted/multiuser workspaces and issues an explicit notice when locally overridden.
- **Agent Teams audit fixes.** Stability audit resolves 32 items and closes the 3 deferred audit findings (F29/F30/F31), significantly improving Teams reliability.
- **Media-job log compaction.** Appends and compacts media-jobs.jsonl content to one entry per terminal job state via atomic tmp-rename, preventing log bloat.

## [0.75.0] - 2026-06-25

### Added
- **Files tab file management.** Drag files from your computer onto the tree to upload them into the project, and right-click any file or folder to **Rename** or **Delete**. All sandbox-checked — upload and rename refuse to clobber an existing name, delete is recursive for folders — and the context menus now show a clear hover highlight on every theme.
- **Per-turn token/cost footer in the GUI.** The Chat and Terminal tabs show the `[tokens: …in/…out · …s · $… session]` line after each completed turn, matching the CLI REPL.

### Changed
- **Desktop launches into a clean session.** A fresh app launch no longer silently inherits the previous conversation's history. The "land back in your last work" auto-resume now applies only to the `--serve`/web surface (reopening a browser tab) and to reconnecting to an actively-busy agent.
- **Deleting the active session activates the most recent remaining session** instead of minting a blank one; a fresh session is minted only when nothing is left.
- **Settings:** removed the retired "Deploy target" section.

### Fixed
- **PDF previews render inline again.** The desktop file-asset handler was serving PDFs as `application/octet-stream`, blanking the in-app viewer; it now sends the correct `application/pdf` (and `application/epub+zip`).
- **Thai text extracted from PDFs.** `PdfRead` re-attaches Thai vowel/tone marks that `pdftotext -layout` orphaned behind spurious spaces (script-level rules, no word lists), and routes a badly-garbled Thai text layer to the vision-OCR path so the model transcribes the rendered glyphs instead of a broken font's wrong characters.
- **media-job log compaction** (thanks @modtanoii). The append-only `media-jobs.jsonl` is compacted to one entry per job when the job reaches a terminal state, via an atomic tmp-rename write.

## [0.74.0] - 2026-06-24

### Changed
- **Gateway proxy is now a single toggle.** The desktop "use the thClaws Gateway" control is one flag (`gatewayProxy`) instead of a per-provider list. On: every gateway-routable provider routes through the gateway for **featured** (priced) models; off: pure BYOK. This fixes the per-provider list bugs — the proxy could get stuck on, re-enable itself on `/reload`, or keep routing after being switched off (the old code re-expanded a partial list to all providers on every load). The Settings checkbox sits on the **Featured (gateway-routable)** section header and is enabled only when a CLI access token is present.
- **Settings API-key modal groups providers like `/providers`** — "Featured (gateway-routable)" vs "Additional (bring your own key)", each provider showing its representative model — so it's obvious which providers the proxy covers.

### Fixed
- **Every credential check now honors the proxy.** A proxy-only user (CLI token, no BYOK key) hit "no API key" in several places even on a featured model the gateway can serve: provider routing, the sidebar ready-badge, **loading a session** recorded against a featured model, **spawning a teammate**, and the **skill-recommended-model** resolver. All now treat a gateway-servable model as ready, falling back to BYOK only when the gateway can't serve it (non-featured model → no gateway 400).
- **Session model switches persist + restore correctly.** Switching model mid-session is written to the session log immediately (a `model` event) instead of only when the next chat turn lands, and a restore reads the **latest** model from the log rather than the creation model in the header — so a switched session reopens on the right provider.
- **Stopped session-log bloat from null snapshots.** `plan`/`goal` snapshots are no longer rewritten as `null` on every turn when none was ever set; only real set/clear transitions are recorded.

## [0.73.0] - 2026-06-23

### Added
- **OS-level Bash confinement (`bash.sandbox`, on by default).** Bash commands (and everything they spawn) now run inside an OS-enforced filesystem boundary — writes confined to the workspace + `/tmp` + package-manager caches, secret dotfiles read-denied — so a malicious or mistaken command can't write outside the project no matter how it's obfuscated. macOS Seatbelt (`sandbox-exec`) + Linux **Landlock** (needs no user namespace, so it works where bubblewrap is AppArmor-blocked; bwrap fallback). Probed at runtime: a host where no confiner can enforce falls back to command-screening with a warning rather than breaking commands. Modes `workspace` (default) / `strict` / `off`; applies to subagent + workflow Bash. A sandbox-denied write appends an actionable hint to the Bash output.
- **`thclaws.parallel([specs])` — genuine workflow fan-out.** Runs subagent specs concurrently (capped at `min(16, cores-2)`); plain `Promise.all` over `thclaws.subagent` stays serial. **Settles** rather than rejects — a failed worker becomes its spec's `fallback` (default `null`) so partial results are kept. Per-future caps are task-local-isolated (no KMS-grant bleed). Plus `thclaws.pollUntil(fn, {interval, timeout, until})` for the submit→poll→done async-job shape.
- **Headless agent tooling.** `thclaws agent new <dir> --pattern static-pipeline|batch-fanout|dynamic` scaffolds a best-practice agent that validates green out of the box; `thclaws agent run <dir> [--workflow X --args {…}] [--dry-tools]` executes an agent's workflow headlessly (Task + MCP registered) for behavioral smoke-testing. `thclaws agent validate` deepened: `py_compile`s `.thclaws/scripts/*.py`, cross-checks MCP/skill requirements, warns on `writePaths`+`Bash`.

### Changed
- **`pre_tool_use` hook gate hardened.** The full, untruncated command is now piped to the gate on **stdin** (`THCLAWS_TOOL_INPUT_ON_STDIN=1`) so a screening hook isn't bypassed by a >8 KB command; new `hooks.fail_closed` makes a gate timeout/error **deny** instead of allow. The gate runs upstream of `bash.sandbox` — complementary layers (policy screening + a hard write floor).
- **KMS sidebar auto-refreshes** after a tool-using turn — a KMS an agent/workflow just created or wrote shows up without a `/reload`.

## [0.72.0] - 2026-06-23

### Added
- **Agent-authoring ergonomics.** Subagent definitions can declare an `output_schema` / `input_schema` (single-line inline JSON or a path to a `.json` file); a workflow `thclaws.subagent({agent})` call that omits a per-call `schema` now validates against the def's schema automatically — one source of truth instead of duplicating it in the workflow JS. `WorkflowRun({args})` passes structured input to a pre-authored workflow as a global `args`, replacing the `TASK.md` side-channel. `thclaws.log(msg)` adds a workflow narrator line for observability. A `writePaths` glob allow-list mechanically confines a subagent's file writes (Write/Edit/office tools) to its lane. New `thclaws agent pack` / `thclaws agent validate` build + lint an agent tarball offline — byte-identical to what `/cloud publish` uploads, so scripts/CI never re-derive the strip rules.
- **Workflow subagent calls fail loud on the wrong surface.** Inside a running workflow, `thclaws.subagent(...)` now errors clearly when the surface has no `Task` tool (e.g. `-p` / `/v1`) instead of silently returning a stub the script would mistake for a real result.

### Fixed
- **Hosted workspaces no longer collide on the same name.** A workspace's route was keyed on the email local-part, which isn't unique — two users sharing a local-part (`name@a.com` + `name@b.com`), or a delete+recreate that left a stale route behind, could surface as "no available server". Routes are now keyed on the unique user id, and workspace deletion cleans up every route it created.

## [0.70.0] - 2026-06-20

### Fixed
- **Gateway-proxied providers no longer show "No API Key".** When you route a provider through the LLM gateway (per-provider proxy toggle + CLI token) without entering your own key, the sidebar used to keep showing "no API key" even though calls worked — and the provider could be silently swapped to a local model. The readiness check now recognises a live gateway route, so a proxied provider reads as ready.
- **MiniMax appears in the API-key settings.** MiniMax had full provider support but was missing from the API-key modal, so there was no way to enter `MINIMAX_API_KEY` from the UI. It now shows up alongside the other providers.
- **Gateway providers no longer go stale on desktop.** Once you've enabled the gateway, newly-shipped gateway-routable providers (e.g. z.ai, xAI, Moonshot, MiniMax) now route through it automatically instead of falling back to bring-your-own-key ("set ZAI_API_KEY"). Previously the desktop kept the snapshot of routable providers saved when you first enabled the gateway; only cloud workspaces refreshed it. (BYOK-only setups — where you never enabled the gateway — are unaffected.)
- **Cleaner system prompt when Agent Teams are off.** With `teamEnabled` unset there's now no mention of teams anywhere in the prompt: the base prompt no longer says "coordinating teammates…" (it keeps the unattended/headless awareness), and the Collaboration-primitives section drops the "Agent Teams — disabled (`teamEnabled: false`)" notice, listing only the two primitives that *are* available (Subagent + WorkflowRun). Naming a disabled feature (and the config flag) was inviting models to reason about it and conflate it with the always-available `Task` subagent tool (one model concluded "there's no Task tool because teamEnabled is false", which is nonsense — the `Task` subagent is unrelated to Agent Teams).
- **`/goal continue` loops can no longer run away.** An auto-continuing goal now has a hard backstop: past an absolute iteration cap (100 firings) or a 1.5× overrun of the token/time budget, the goal is auto-blocked and the loop stops — regardless of whether the model marks it terminal. Previously the only automatic stop was the model itself calling `MarkGoalComplete`/`MarkGoalBlocked` (the budget was a soft prompt nudge), so an unattended loop could burn unbounded cost.

### Added
- **Parallel tool calls run concurrently.** When the model emits two or more independent, read-only tool calls in one turn — several `Read`/`Grep`/`Glob`/`Ls`, or a fan-out of `Task` subagents — they now run **concurrently** instead of one-at-a-time. Wall-clock drops from the sum of their durations to the slowest single one (a big win for parallel research/audit subagents). Mutating tools (`Write`/`Edit`/`Bash`) still run strictly in order.

## [0.69.0] - 2026-06-20

### Added
- **`pre_tool_use` hooks can now block a tool call.** A hook that exits `2` denies the call — the tool never runs and the hook's stderr is shown to the model as the reason; any other exit code still allows it (existing audit-only hooks are unchanged). This turns the audit hook into an enforceable gate — e.g. confine `bash` to the working folder instead of only logging it. Reference policy: `examples/hooks/audit-bash-confine.sh`.

### Changed
- **`bash` no longer exposes platform credentials to the shell.** Platform secrets (the gateway access key, the multiuser identity secret, the cloud token) are always stripped from a command's environment; in a shared/multiuser session the owner's provider API keys are stripped too — a `printenv` in `bash` can no longer read them.

### Security
- Hardening for thClaws.cloud hosted + shared workspaces: HttpOnly session cookies (no workspace-subdomain token theft), credential stripping before the runner, per-workspace identity secrets, per-workspace daily spend caps, and gateway per-user rate limiting.

## [0.68.0] - 2026-06-19

### Added
- **Marketplace now covers four types — skills · MCP · plugins · subagents.** Subagents (agent defs) are a new installable catalog type: `/subagent marketplace | search | info | install` pulls a single `.md` agent def into `.thclaws/agents/`. A unified **`/marketplace`** command opens a GUI browser with a tab per type, search, and one-click install.
- **Author agent defs in the GUI — `/agent new` / `/agent edit`.** GUI-only commands open a modal to edit an agent's YAML frontmatter and system prompt, saving a project override at `.thclaws/agents/<name>.md`.
- **Gated tool groups + GUI Shell authoring tools.** Tools can be registered but hidden until a skill opens their gate; the GUI-shell authoring tools are now surfaced lazily via the bundled `gui-shell` skill instead of a ~3KB always-on system-prompt block.

### Changed
- **Gateway overlay → `gateway.thclaws.cloud` + CLI-token auth.** The thClaws Gateway base URL is the consolidated `gateway.thclaws.cloud`, and a thClaws.cloud login (CLI token) is now accepted directly — no separate `gw_v1_` key needed when you're signed in.

### Fixed
- **Native Gemini provider no longer 400s on tool schemas** ([#172](https://github.com/thClaws/thClaws/issues/172)). Tool schemas are sanitized to Gemini's `Schema` subset before sending — strips `$schema` / `additionalProperties` / `propertyNames` (recursively), drops non-string `enum`s (e.g. `[0,1,2]`), keeps string enums.
- **WYSIWYG `.md` editor preserves HTML comments and images** on edit/save. Wrapper markers like `<!-- img:foo -->` and `![alt](src)` survive the round-trip (custom comment node + Image extension) instead of being silently dropped.
- **Chat no longer yanks you to the bottom while reading history** ([#170](https://github.com/thClaws/thClaws/issues/170)). Streamed tokens only auto-scroll when you're already pinned to the bottom.
- **Compact running indicator** ([#171](https://github.com/thClaws/thClaws/issues/171)). The RunningChip is now a narrow dot + elapsed time (a static dot when idle); session id and progress move to the hover tooltip so it no longer pushes header items off-screen.

## [0.67.0] - 2026-06-17

### Added
- **Provider tiers — Featured vs Additional.** The ten first-class providers (OpenAI, Anthropic, Gemini, xAI, DeepSeek, DashScope, Moonshot/Kimi, z.ai, MiniMax, OpenRouter) are now grouped as **Featured** and listed before everything else in the model picker and `/providers`. On thClaws.cloud, gateway-routed sessions show only Featured providers (the gateway-routable set); bring-your-own-key sessions still see the full catalogue. Featured-provider pricing is enforced to come from an official source (the provider's own pricing API, LiteLLM, or a vendor pricing page) — a missing price is now a release-blocking error instead of a silent placeholder.
- **The `--serve` web UI is now mobile-friendly.** Opening a thClaws server from a phone browser gets a real mobile layout: the sidebar becomes a slide-in drawer (hamburger in the tab bar), the tab strip scrolls and collapses to icons, the Files view stacks tree-over-editor, modals fit narrow screens, touch targets are larger, text inputs no longer trigger iOS zoom-on-focus, and the terminal accepts a tap to bring up the on-screen keyboard.

### Changed
- **Removed the `agentic-press` provider.** Its `ap/` model ids no longer resolve.

### Fixed
- **`openrouter/fusion+` works from `/model` and the picker** ([#167](https://github.com/thClaws/thClaws/issues/167)). `/model openrouter/fusion+` in the CLI/terminal no longer reports "unknown model", and selecting `openrouter/fusion+` from the `/model ` popup now opens the Fusion config modal (matching the sidebar picker).
- **Mobile Chrome no longer clips the layout** ([#168](https://github.com/thClaws/thClaws/issues/168)). The root container uses `100dvh`, so the tab bar and input aren't hidden behind the browser address bar.
- **Browser/Shell tabs appear over high-latency connections.** Their visibility flags now ride the initial-state push the server sends on every (re)connect, instead of a mount-time request that a slow WebSocket (e.g. through a tunnel) could silently drop.
- **`zai/glm-5.2` reports its real 1M context** ([#161](https://github.com/thClaws/thClaws/issues/161)) instead of the 131k provider-default fallback.

## [0.66.0] - 2026-06-17

### Added
- **Two new providers — Moonshot AI (Kimi) and xAI (Grok).** Both are OpenAI-compatible and desktop-direct (bring your own key): drop `MOONSHOT_API_KEY` / `XAI_API_KEY` into Settings, then `/model moonshot/kimi-k2.6` or `/model xai/grok-4.3` (bare `grok-*` ids route too). New Kimi / Grok releases are picked up automatically by the catalogue refresh.
- **Credential-aware default provider.** On a fresh start — when no model is explicitly pinned — thClaws now selects the first provider you actually have a key (or gateway route) for, in order DashScope → OpenAI → Anthropic, instead of always defaulting to Anthropic.

### Changed
- **Refreshed per-provider default models:** DashScope → `qwen3.7-max`, OpenAI → `gpt-4.1`, Gemini → `gemini-3.5-flash`, z.ai → `glm-5.2`. These apply both to the startup default and to `/provider <name>` switches.
- **`make catalogue` now discovers z.ai, Moonshot, and xAI** from their live `/v1/models`, so new GLM / Kimi / Grok models appear without a hand-edit (z.ai was previously missing entirely — `glm-5.2` never showed up).

### Fixed
- **Shell tab accepts keyboard input immediately on tab switch** ([#166](https://github.com/thClaws/thClaws/issues/166)). Under the wry/Chromium webview the just-unhidden terminal wasn't reliably focusable in the same frame, so keystrokes were silently dropped until you clicked inside it. Focus is now deferred a frame (with explicit click-to-focus as a fallback).

## [0.65.0] - 2026-06-17

### Added
- **Multiuser `--serve` mode** (dev-plan/42). `thclaws --serve --multiuser` hosts many authenticated users from one pod, each in their own `workspace-<id>/` working directory with isolated session history and files. The agent definition (AGENTS.md, KMS, skills, MCP config) is seeded read-only per user; HMAC-signed `X-Thclaws-User` identity routes every request — and every tool's path resolution — to the right per-user session; the gateway is forced (no BYOK). This powers thClaws.cloud **workspace sharing**: one owner-billed pod, many guests; the owner edits the agent and publishes updates to everyone, and guests can wake a sleeping shared workspace themselves.

### Fixed
- **Clear error when an image is sent to a text-only model** ([#164](https://github.com/thClaws/thClaws/issues/164)). Reading an image or an image/scanned PDF (e.g. `PdfRead` rendering blueprint pages) and feeding it to a model that can't see images (DeepSeek v4, most non-`-vl` Qwen, etc.) previously failed with a bare upstream `HTTP 400`. The OpenAI-compatible provider now detects image content in a rejected request and appends an actionable hint — switch to a vision model (e.g. `dashscope/qwen3-vl-plus`), or extract the PDF/image to text/KMS first and query that.

## [0.64.0] - 2026-06-16

### Fixed
- **Telegram: recover a stuck conversation without restarting** ([#164](https://github.com/thClaws/thClaws/issues/164)). When a provider error (e.g. HTTP 400 once the history grows too large) kept replaying every turn, the only fix was killing `thclaws --telegram` from the console. You can now send `/new` (or `/reset` / `/clear`) in the chat to wipe that agent's conversation and start fresh. Tolerates the group-mention suffix (`/reset@yourbot`).

## [0.63.0] - 2026-06-15

Engine support for thClaws.cloud **shared agents** (company-owned agents
several people use). Cloud-only and fully dormant on desktop — running
locally is unchanged.

### Added
- **Shared-agent mode** (activated by `THCLAWS_SHARED_AGENT_DIR`, set only
  by the hosted runtime): instructions lock to the company `AGENTS.md`,
  the KMS mounts read-only, the gateway is forced (no BYOK), and member
  scopes (`~/.config/thclaws`, `~/.claude`, working dir) are ignored, so a
  member can't override the company agent. Skills/commands/MCP load from
  the shared brain (with optional strict mode). All of this is gated on
  that env var being set — when it isn't (every desktop install), config,
  instruction, KMS, skill/command, and provider resolution behave exactly
  as before.

(The cloud control-plane half of shared agents — the dashboard, launch,
brain upload, members, and billing — lives in the thClaws.cloud service,
not this binary.)

## [0.62.0] - 2026-06-15

KMS ↔ Open Knowledge Format (OKF) interchange.

### Added
- **Import/export KMS as OKF bundles.** New `/kms export-okf <name> [<out-dir>]` writes a knowledge base as a conformant [Open Knowledge Format](https://github.com/GoogleCloudPlatform/knowledge-catalog) v0.1 bundle (defaults to `./<name>-okf/`); `/kms import-okf <bundle-dir> <name> [--project]` creates a new KMS from any OKF bundle. OKF is Google's open spec for knowledge-as-markdown — the same "LLM wiki" shape a KMS already uses — so this is a clean round-trip for shipping a KMS across teams/agents or pulling external bundles in. It's an adapter, not a storage change: the on-disk KMS format is unchanged.
- **OKF import/export from the sidebar.** Right-click the desktop sidebar's **Knowledge** section header for "Import OKF bundle…" (name + scope, then a native folder picker) and per-KMS "Export OKF bundle" (native folder picker). A status line confirms the result; imports refresh the KMS list immediately.

The adapter maps `category:`↔`type:`, `topic:`↔`description:`, comma `tags`↔YAML list, `sources/`↔`references/`, and converts `[[wikilinks]]` to markdown links on export; KMS-specific frontmatter rides along verbatim so round-trips are lossless. Import is permissive per the OKF spec (tolerates unknown types, missing fields, broken links, and concepts anywhere in the tree).

## [0.61.0] - 2026-06-14

OpenRouter Fusion: fixed, plus a configurable variant with a GUI panel.

### Added
- **`openrouter/fusion+` — configurable OpenRouter Fusion.** Selecting it in the model picker opens a config modal to tune the deliberation panel: `analysis_models` (1–8 panel models), judge model, outer/orchestrator model, `max_tool_calls`, `max_completion_tokens`, `temperature`, reasoning effort, and `tool_choice` (`auto` — coexists with the agent's own tools — or `required`). The engine calls the outer model with the `openrouter:fusion` tool attached, carrying these parameters; unset fields fall through to OpenRouter's defaults. Config persists to `.thclaws/settings.json` under `openrouterFusion`, so it works headless / `--serve` too, not just the GUI.

### Fixed
- **`openrouter/fusion` and `openrouter/auto` 404'd with "No endpoints found that support tool use".** thClaws stores OpenRouter ids as `openrouter/<vendor>/<model>` and strips the leading `openrouter/` before the wire call — but these router models' vendor *is* `openrouter`, so stripping sent the vendor-less `fusion`/`auto`, which routes to nothing that supports tools. The prefix is now kept when stripping would leave a bare single segment (a real OpenRouter id is always `vendor/model`). Other providers' prefixes (`lmstudio/`, `dashscope/`, …) are unaffected.

## [0.60.0] - 2026-06-14

More providers + media models, and a batch of Linux team/serve fixes.

### Added
- **TokenRouter provider** ([#162](https://github.com/thClaws/thClaws/issues/162)) — first-class, OpenAI-compatible access to TokenRouter's unified gateway (300+ models). Use `tokenrouter/<vendor>/<model>` (e.g. `tokenrouter/anthropic/claude-opus-4.7`); key `TOKENROUTER_API_KEY`, base overridable via `TOKENROUTER_BASE_URL`. Models populate the picker beyond the generic `oai/` slot.
- **HappyHorse video models (DashScope).** `happyhorse-1.0-t2v` (text→video) and `happyhorse-1.0-i2v` (image→video) added to `TextToVideo` / `ImageToVideo` and the Media Studio shell, with a 720P/1080P `resolution` option. Needs `DASHSCOPE_API_KEY`. Local source images for i2v are sent inline (base64 data URI) — no upload step.

### Fixed
- **Team + serve mode on Linux** ([#163](https://github.com/thClaws/thClaws/issues/163)): (1) response text no longer vanishes under multi-subscriber streaming — the `ViewEvent` broadcast buffer was 256, now 2048, and lag is logged in the forwarders instead of dropped silently; (2) teammate cleanup `pkill -f -- "--team-dir …"` now works (the leading `--` was being parsed as an option — broke on Linux *and* macOS); (3) reasoning-only assistant turns serialize `content: ""` so OpenAI-compatible providers (DeepSeek, …) don't reject them with HTTP 400.
- **Media Studio source-image picker.** Clicking a gallery image in Image Edit / Image → Video now sets it as the source (the "click a gallery item" hint finally does something).

## [0.59.0] - 2026-06-14

Built-in media generation — multi-provider image + video tools, plus a Media Studio GUI shell to drive them.

### Added
- **Provider-abstracted image tools.** `TextToImage` / `ImageToImage` are no longer Gemini-only — choose `flash`/`pro` (Gemini), `gpt-image-2` (OpenAI), or `qwen-image-2.0` / `-pro` (Alibaba Qwen, strong at multi-image edits + text rendering). The provider is inferred from the model.
- **Built-in video tools.** New `TextToVideo` / `ImageToVideo` (Veo 3.1 fast/quality/lite) with an async submit→poll job model and a `MediaJobStatus` tool. Jobs persist to `.thclaws/media-jobs.jsonl` and resume across restarts; clips land in `output/vid-*.mp4`.
- **Media Studio GUI shell.** A built-in shell (UI tab) for image + video: mode switch (text→image / image edit / text→video / image→video), provider + model picker, parameters, and a gallery with a lightbox. The gallery is disk-backed — it shows everything under `output/`, newest first, not just the current session.
- **Theme-aware GUI shells.** chatbot, session-explorer, and Media Studio now follow the app's Light/Dark/System theme (the shell bridge exposes `thclaws.ui.theme` / `onTheme` and mirrors it onto `data-theme`). A starter template (`thclaws-gui-shell-template`) ships the correct pattern for new shells.

### Changed
- **Media tools are opt-in via `mediaToolsEnabled`** (alias of the legacy `imageToolsEnabled`; the flag now covers image *and* video) — but the Media Studio shell auto-enables them, so it works without toggling settings.
- **GUI shells can drive tools through the approval flow.** A shell's `callTool` for a tool that costs money now raises the normal approval modal instead of being rejected outright.

### Fixed
- **Veo `durationSeconds` clamped to 4–8** — the API rejects values outside that range (a 2s request 400'd).
- **Media Studio readability.** A lightbox backdrop with `display: flex` outranked its `hidden` attribute and dimmed the entire shell; it's now gated on the attribute.
- **Clearer image-gen errors.** A Gemini "HTTP 200 but no image" now reports the `finishReason` / safety-block / raw body instead of an opaque "missing parts".

## [0.58.0] - 2026-06-14

### Added
- **Settings-menu side flyouts.** The Instructions (Global / Folder
  AGENTS.md), channel connectors (LINE / Telegram / Messenger), and
  Appearance (Light / Dark / System) groups now fan out into compact
  left-side popups instead of taking a row each — a much shorter menu.

### Changed
- **Default startup tab is now Chat** (was Terminal). Agents that pin a
  tab via `guiShell.tabDefault` (e.g. a gui-shell workspace) still open
  to their shell.
- **Files tab resolves per-user subdomain URLs.** `workspacePrefix` now
  derives the file-asset prefix for hosted workspaces served at
  `<handle>.thclaws.cloud/<slug>/`, in addition to the legacy
  `/u/<handle>/<slug>/` path scheme — so chapter images and other
  relative assets load correctly under either URL form.

## [0.56.0] - 2026-06-14

### Added
- **EPUB preview in the Files tab.** `.epub` files now render in-app via
  epub.js — scroll-per-chapter with Prev/Next and arrow-key navigation, a
  chapter label, and light/dark theming. Previously an EPUB opened as
  "Error reading file" (it is a zipped XHTML bundle, not text); the backend
  now serves it off `/file-asset` like PDF/audio/video.
- **`PdfCreate` / `EpubCreate` font option.** Both tools accept
  `font: "sans"` (default — Noto Sans + Noto Sans Thai) or `"serif"`
  (Noto Serif + Noto Serif Thai). The serif faces ship with full Thai
  shaping (GSUB/GPOS) for long-form / book typography; the PDF embeds the
  chosen family and the EPUB switches its `@font-face` set accordingly.

### Security
- **Bumped `@xmldom/xmldom` to 0.8.13** (transitive via `epubjs`), fixing
  five high-severity advisories — an uncontrolled-recursion DoS and several
  XML-injection serialization issues — present in the pinned 0.7.13.

## [0.54.0] - 2026-06-13

### Fixed
- **Browser tab chat sidebar now handles `AskUserQuestion`.** When the
  model asked a question during a turn driven from the Browser tab's
  sidebar, the sidebar ignored the prompt — the question never showed
  and the turn hung with no way to answer. The sidebar now surfaces the
  question and routes the next input to the pending-ask responder
  (mirroring the Chat tab).

### Changed
- **Browser content extraction prefers the page snapshot over
  screenshots.** When the browser tools are active, the model is now
  steered to use `browser_snapshot` (the page's text / accessibility
  tree) as the primary source for reading or extracting content —
  translating headlines, scraping lists, pulling article text — and to
  fall back to `browser_take_screenshot` only for visual-only content
  (charts, canvases, image-embedded text). Reading text off pixels was
  slower, lossier, and mis-read characters.

## [0.53.0] - 2026-06-13

### Fixed
- **Browser automation now works reliably after the first run.** On
  machines without Playwright's own Chromium installed, the engine fell
  back to driving *branded* Google Chrome over CDP, which intermittently
  failed playwright-mcp's init with `protocol error
  (Browser.setDownloadBehavior): Browser context management is not
  supported` — so every browse after the first failed until a restart.
  The engine no longer hands a branded browser to the CDP live-view path
  by default; playwright-mcp self-launches its own browser (reliable).
  Opt back into branded-over-CDP with `THCLAWS_BROWSER_ALLOW_BRANDED=1`;
  the live view / takeover otherwise uses Playwright's own Chromium
  (`npx playwright install chromium`).
- The engine-owned Chromium is now killed on quit (with a cookie flush)
  instead of orphaning, and a stray orphan from a previous run is reaped
  on the next launch rather than re-attached to.

### Added
- **Wider default browser viewport** — sessions render at desktop-width
  1920×1080 instead of playwright-mcp's narrow 1280×720 default.
  Override with `THCLAWS_BROWSER_VIEWPORT="W,H"`.
- Browser automation chapter in the user manual (EN + Thai) and a
  `browser.md` engine-internals topic in the technical manual.

## [0.52.0] - 2026-06-13

### Added
- **Browser cookies/logins persist across restarts.** The
  engine-owned Chromium profile survives browser and (cloud) pod
  restarts — on cloud the profile moves to the workspace PVC, and a CDP
  cookie snapshot/restore (`Storage.getCookies`/`setCookies`) closes
  chromium's ~30s on-disk-flush window so a login isn't lost to an
  abrupt kill. The profile is stripped from agent publishing, so
  cookies never leak into a shared agent.
- **opencode-go**: `minimax-m3`, `qwen3.7-max`, `qwen3.7-plus` added to
  the wire-format routing tables (thanks @modtanoii, #158).
- Server-level integration tests for the `/upload?dir=` endpoint —
  subdir routing, collision suffix, path-traversal rejection (thanks
  @modtanoii, #159).

### Fixed
- **Headless Telegram now honours `auto` permissions (#160).**
  `thclaws --telegram` hardcoded approval-routing (`telegramgated`), so
  `--accept-all` / `--permission-mode auto` / `settings.json
  permissions:auto` were all silently ignored and every tool call
  demanded an inline-button tap — no-prompt auto was impossible. It now
  resolves the mode from config; explicit `auto` runs with no prompts.
  Also: the CLI REPL's `/permissions auto|ask` now persists to
  `.thclaws/settings.json` (matching the GUI), so the setting survives
  a restart.
- `browser_cdp` is no longer behind the `gui` feature — it was
  referenced unconditionally by `mcp.rs`/`config.rs`, breaking the
  `thclaws-cli` build.

## [0.51.0] - 2026-06-12

### Added
- **Live browser view + native input (CDP, docs/browser slice 3).**
  The engine now owns Chromium: at browser-MCP bootstrap it reserves a
  DevTools endpoint and hands playwright-mcp `--cdp-endpoint`, so the
  agent's tools and the human share ONE browser. Chromium itself
  launches lazily — on the first browser tool call or takeover — so a
  headed desktop doesn't pop a window at app start and an idle cloud
  pod pays nothing. The engine's own CDP session powers the Browser
  tab when takeover is on:
  - `Page.startScreencast` → a true live view (JPEG stream,
    ack-backpressured) instead of ~1 fps click-through screenshots
  - `Input.dispatchMouseEvent` / `Input.insertText` /
    `dispatchKeyEvent` → native click / scroll / whole-string typing
  - `Runtime` console + exception events and top-frame navigations
    stream into the activity feed / URL line
  - the engine re-attaches to a still-running Chromium after a
    restart (DevTools endpoint persisted next to the profile), so
    logins survive engine restarts; profiles live OUTSIDE the
    workspace so sessions can never leak into a published agent
  - graceful fallback everywhere: no Chromium found / CDP launch
    failure → playwright-mcp self-launches and the screenshot +
    MCP-input takeover keeps working; `THCLAWS_BROWSER_CDP=0`
    disables the whole mode

### Added
- **Vision models can now SEE MCP tool images** — most importantly
  `browser_take_screenshot`. `McpTool` gained a `call_multimodal`
  override that preserves `{type:"image"}` content blocks (the plain
  text path silently dropped them), so the agent can read canvases,
  charts, and visual layouts the accessibility snapshot can't express.
  5 MB per-result image cap (oversize degrades to a text note);
  text-only MCP results behave exactly as before; non-multimodal
  providers still get the text blocks.

## [0.50.0] - 2026-06-12

### Added
- **Browser tab: interactive takeover (Phase 2 slice 2).** A "🖱 Take
  over" toggle makes the screenshot panel a remote control for the
  managed browser — click anywhere on the page (object-contain-aware
  coordinate mapping), scroll with the wheel, type into the focused
  field, press Enter/Tab/Esc/⌫, and navigate via a URL bar — so cloud
  users can log into sites themselves before handing the session to
  the agent. Backed by a new `browser_input_call` IPC arm with a
  STRICT tool allowlist (coordinate input + navigation only; no
  evaluate/run_code/file_upload) running directly on the managed MCP
  client, and the managed server now starts with `--caps=vision` for
  the coordinate tools. Verified end-to-end: click → per-char typing
  fires real input events → screenshot reflects the page.

### Changed
- **Browser automation is ON by default** (`browserEnabled` defaults to
  `true`). Every workspace gets the managed Playwright browser + the
  Browser tab without configuration. Graceful where it can't work: the
  injection is skipped when the launch command isn't on PATH (node-less
  desktops see the tab's setup hint instead of per-session spawn
  errors), and `"browserEnabled": false` opts out entirely.

### Fixed
- Cloud runner image: the managed browser server is pinned to the
  playwright-bundled chromium (`--browser chromium`) — playwright-mcp
  defaults to branded Google Chrome, which isn't in the image — and
  the server binary name is `playwright-mcp` (the package renamed its
  bin from `mcp-server-playwright`).

## [0.49.0] - 2026-06-12

### Added
- **Cloud browser automation (docs/browser Phase 2, slice 1).** The
  runner image now ships a working browser stack: `@playwright/mcp`
  preinstalled (no npm-registry hit per pod cold start), chromium
  installed to a shared `PLAYWRIGHT_BROWSERS_PATH=/ms-playwright`
  readable by the runtime user (previously root-only — the existing
  playwright install was unusable from pods), and
  `THCLAWS_BROWSER_MCP_CMD=mcp-server-playwright --no-sandbox` so the
  engine launches the image-pinned server. Engine honours that env as
  a full launch-command override (desktop default stays
  `npx -y @playwright/mcp@latest`); `--headless` is auto-appended on
  displayless environments. With `browserEnabled` in a cloud
  workspace's settings, the Browser tab's screenshot panel becomes the
  headless browser's window. Live interactive takeover (CDP screencast
  + remote input) is the next slice.
- **Engine-managed browser automation (Playwright MCP, Phase 0+1).**
  `"browserEnabled": true` in settings.json injects the official
  `@playwright/mcp` server as an engine-managed MCP config — the agent
  gains 23 `browser__*` tools (navigate / click / type / snapshot /
  network / …) with no `/mcp add` and no first-spawn prompt (the
  `engine_managed` flag is serde-skipped, so a cloned repo's mcp.json
  can never claim the bypass). Headed by default on desktop — a real
  Chromium window beside the app, browse normally and let the agent
  take over — headless automatically on cloud runners / displayless
  Linux (`browserHeadless` overrides). New **Browser tab** (visible
  only when enabled) shows the managed-server status, an npx setup
  hint, and a live feed of every browser tool call + result. Requires
  Node.js (`npx`) on PATH.
- **Browser tab: chat sidebar + live page screenshots.** A compact
  agent chat docked in the tab (same conversation as Chat — direct the
  takeover without switching tabs), and a screenshot panel that
  auto-captures ~1s after each browser action (plus a manual 📷
  button). Captures run directly on the managed MCP client over a new
  `browser_screenshot_get` IPC arm — no agent loop, no tokens, works
  mid-turn — via `McpClient::call_tool_raw`, which preserves the image
  content blocks the text path drops.

## [0.48.0] - 2026-06-11

### Added
- **`/upload?dir=` — dir-targeted, silent file staging.** The serve
  upload endpoint now accepts an optional `?dir=<rel>` query param that
  writes the upload into a specific workspace subfolder (sandboxed
  against escape) instead of `uploads/`, and skips the chat-turn
  synthesis. Lets a GUI shell stage files for itself — e.g.
  book-author's new Sources tab dropping the author's notes straight
  into `raw/` — without the agent reacting to the drop. Default (no
  `dir`) behavior is unchanged.

## [0.47.0] - 2026-06-11

### Added
- **EpubCreate** — new native tool rendering markdown to a reflowable
  EPUB 3 e-book: chapter splitting at headings (each H1 → its own
  spine item + navigation entry), markdown→XHTML (GFM tables,
  strikethrough, task lists, footnotes), embedded images, optional
  cover, EPUB 3 `nav.xhtml` + EPUB 2 `toc.ncx` fallback, and embedded
  Noto Sans + Noto Sans Thai (`@font-face`) so Thai renders on readers
  with no Thai font. Validated against the official EPUBCheck (3.3).
- **GUI shell `thclaws.ui.*` bridge API** — full-screen integration for
  shell authors: `exitFullscreen()`, `claimExitControl()`,
  `onFullscreen(cb)`, `isFullscreen`. A shell can render its own
  full-screen exit control and have the host suppress its fallback chip.

### Changed
- **Full-screen UI exit no longer occludes the shell.** The host's exit
  affordance was a fixed top-right chip permanently covering the shell's
  corner. Now: a brief auto-dismissing toast names the ⌘⇧U/Ctrl⇧U escape
  on entry, and the clickable fallback chip is revealed only on
  top-right hot-corner hover. The keyboard escape stays host-owned. All
  built-in shells (chatbot, session-explorer) render their own header
  exit button via the new API.

### Fixed
- pdf_create module docs: dropped the stale "no OpenType shaping" note —
  v2 shapes every run through rustybuzz (GSUB/GPOS).

## [0.46.0] - 2026-06-11

### Added
- **PdfCreate v2** — book-quality markdown→PDF: HarfBuzz (rustybuzz)
  text shaping so Thai stacked tone marks render correctly, ICU4X
  Thai word-boundary line breaking, real glyph metrics, embedded
  Noto Sans Bold/Italic + Noto Sans Thai Bold, bordered GFM tables,
  lists with hanging indents, blockquote bars, shaded code blocks,
  centered fit-width images with alt-text captions, `n / N` page
  footers, Thai-readable (UTF-16) PDF outline bookmarks.
- PdfCreate `content_path` input — render a markdown file directly
  (books never round-trip through the model context); relative image
  paths resolve against the file's directory. `page_break_h1` starts
  each chapter on a fresh page; `outline_depth` controls the sidebar
  (default: chapters only).
- WorkflowRun `script_path` input — execute pre-authored agent
  workflow scripts (book-author `/draft-all-parallel`) without the
  authoring step.
- Native Gemini image tools route through the thClaws Gateway on
  hosted runners, sniff the actual image format (PNG/JPEG/WEBP), and
  register on every surface (GUI, serve, REPL, print mode, workflow
  workers); they also follow `imageToolsEnabled` across config
  reloads.
- Gateway overlay covers every cloud-routable provider (DashScope,
  Qwen-Cloud, Z.ai, DeepSeek, MiniMax, ThaiLLM) with strict
  catalogue-priced metering; unpriced models are hidden from model
  pickers when gateway-routed.

### Fixed
- Files tab: PDFs render inline (served as `application/pdf` off
  `/file-asset`), markdown-preview images load on hosted workspaces,
  and the session sidebar survives fullscreen remounts.
- Engine no longer requires a native provider API key when the
  gateway overlay carries the credential.


## [0.45.0] — 2026-06-09

Security hardening on the gui-shell bridge — defence-in-depth that
closes a script-breakout vector — plus a sweep of community-facing
housekeeping: 24 retroactive entries on CONTRIBUTORS.md and the v0.32
landing-page callout finally retired.

### Security

- **gui-shell: escape `</` in injected values to prevent HTML script
  breakout ([#157](https://github.com/thClaws/thClaws/pull/157)).**
  `inject_inline_bridge_with_id` and `inject_mode_b_head_with`
  splice JSON-serialized values (`shell_id`, `session_id`, `ws_url`)
  into `<script>` tags. JSON-escaping handles quotes and backslashes
  but does NOT escape `</`, and the HTML tokenizer scans for the
  literal byte sequence `</script>` regardless of JS-level escaping.
  A shell manifest containing `</script>` in its `id` could close
  the injected `<script>` tag prematurely and break out. Fix: post-
  JSON `.replace("</", "<\/")` on every injected value plus the
  bridge runtime — `<\/` is invisible to the HTML tokenizer, valid
  JSON, and byte-equal to `</` in JS at runtime. Real (if low-
  severity) defence on the `--serve` and hosted-cloud surfaces; the
  marketplace gui-shells story makes this matter more over time.
  PR by @JonusNattapong.

### Changed

- **README + landing page: retired the "new in v0.32" callout.**
  The Shell-tab + Claude-Code-inside-thClaws callout had been the
  top of the README and `thclaws.ai` landing page for 12 versions
  — "new in v0.32" stopped reading as fresh ten releases ago. The
  Shell story is permanent product surface and is covered in the
  Features section and ch26 of the manual. Replaced with the
  existing showcase as the first content section.

### Community

- **24 retroactive contributor credits.** CONTRIBUTORS.md was 5
  entries deep but the merged-PR graph showed roughly five times
  that. Audited every login on every merged PR and backfilled 22
  PR senders in chronological order (oldest: @bombman's PR #2;
  biggest counts: @parintorns 9 PRs, @siharat-th 8 PRs). Also
  credited @triok-t (co-author on PR #16) and @dome (PR #110
  closed → adopted into #113 by @mozeal). 29 community
  contributors listed now. Thank-you comments posted on each
  new contributor's most recent merged PR.

## [0.44.0] — 2026-06-09

DashScope routing fix — the model picker, the catalogue, and the
engine's prefix detection finally agree on `dashscope/<model>` as
the canonical form.

### Fixed

- **DashScope model picker double-prefix bug
  ([#156](https://github.com/thClaws/thClaws/issues/156)).** The
  catalogue stored DashScope rows with a `dashscope/` prefix so
  heterogeneous Alibaba-hosted families (qwen, deepseek, glm, kimi,
  qwq) all route through one provider, but `ProviderKind::detect()`
  had no `dashscope/` arm — it only matched bare `qwen*`/`qwq-*`.
  Three knock-on bugs: `/model dashscope/qwen-max` failed with
  "unknown model provider"; the sidebar picker double-prefixed to
  `dashscope/dashscope/qwen-flash` when switching across providers;
  `/provider dashscope` warned "no catalogue entry for 'qwen-max'"
  because catalogue keys were prefixed but the default was bare.
  Brought DashScope in line with the established prefix-routing
  pattern (`zai/`, `qc/`, `ap/`, `oai/`, `lmstudio/`): added the
  `dashscope/` arm to `detect()`, moved the default model to
  `dashscope/qwen-max`, and `with_strip_model_prefix("dashscope/")`
  on the provider strips the prefix before reaching Alibaba's
  upstream. Bare `qwen-*`/`qwq-*` ids still route to DashScope for
  backward compat with pre-prefix settings. Reported by @pok29dev
  with the load-bearing pointer that `dashscope/<model>` was the
  right canonical shape.

### Changed

- **Docs: `thclaws cloud …` CLI surface marked deprecated.** The
  engine removed every `thclaws cloud …` shell subcommand back at
  v0.36; ch27 of the user manual (English + Thai) had been showing
  the old CLI all along. Rewritten to the in-session `/cloud` slash
  flow. Settings → thClaws.cloud paste-in-GUI is the only auth
  surface; every other op runs as a slash command inside an open
  session.

### Community

- **@pok29dev added to [CONTRIBUTORS.md](CONTRIBUTORS.md)** for
  the DashScope picker bug report (#156).

## [0.43.0] — 2026-06-09

Catalog-policy reversal + Asian-provider pricing fill-in. The picker
now shows every model the engine can route to (matches pre-v0.41
behaviour) and dashscope-hosted families finally get a price tag
instead of an empty cost column.

### Changed

- **Model picker shows every routable model again.** v0.41.0
  introduced an `is_listable()` filter that hid catalogue rows
  without published pricing from the sidebar / `/model` / `/models`
  / `/v1/models`. v0.42.0 propagated it to the last hold-out
  (`build_all_models_payload`). End result: dashscope alone lost
  98 chat models from the picker — entire families (qwen-coder-plus,
  qwen-flash, deepseek-v4-*, kimi-k2.6, qwen-mt-*) that work fine
  for BYOK users were invisible. Reversed: the only filter on the
  picker is `chat != Some(false)` again. Missing pricing means the
  catalogue refresh has a gap to fill, not that the model is
  unusable. Cloud-gateway-routed traffic still gets a strict 400
  reject when the cloud `model_pricing` table is missing a row —
  bill-shock guard intact, just not surfaced as a picker filter.

### Fixed

- **Asian-provider pricing fill-in.** Added
  `MANUAL_PRICING_OVERRIDES` to `scripts/refresh-model-catalogue.py`
  for dashscope-hosted models LiteLLM doesn't track — sourced from
  https://www.alibabacloud.com/help/en/model-studio/model-pricing
  (International tier, highest token-band for a safe upper bound).
  Longest-prefix matching so dated variants
  (`qwen3-max-2025-09-23`) pick up their base family's price
  without enumerating every dated id. dashscope priced coverage
  jumped from 11/109 → 39/109 in one refresh: qwen-max, qwen-plus,
  qwen-turbo, qwen-flash, qwen3-max, qwen3-coder-plus/flash,
  qwen3-vl-flash, deepseek-v3.2, kimi-k2.6, glm-5.1, qwq-plus.

- **Sidebar showed models that `/models` hid.** Symptom of the
  v0.42.0 `is_listable()` gap that this release reverts entirely.
  Sidebar = REPL `/models` = HTTP `/v1/models` again.

### Added (developer-facing)

- **`make audit-pricing` + pre-cut gate.** Every priced row in the
  catalogue must carry a `litellm:` / `manual:` / `derived:` tag
  in its `source` field. `scripts/audit-pricing-provenance.py`
  walks the catalogue and fails the release if any priced row is
  opaque. Wired into `make release` right after the catalogue
  refresh and before the version bump, so a tag can't go out with
  un-attributed pricing. Bill-shock guard: every cent the gateway
  later debits has to trace back to a documented vendor source.
  Audit-clean at this release: 302 priced rows, all tagged
  (274 litellm / 28 manual / 12 derived).

## [0.42.0] — 2026-06-08

Small cleanup release on top of v0.41.0 — one community PR plus two
hosted-workspace UX fixes that surfaced once v0.41.0's tighter cloud
gating went live.

### Fixed

- **Terminal cursor position resets on line-clear events
  ([#153](https://github.com/thClaws/thClaws/pull/153)).** Two paths
  cleared `lineBuffer` without resetting `cursorPos` — Escape on the
  slash-command popup, and the engine's `terminal_clear` event. The
  next keystroke landed at the right character but the visible caret
  drifted past it. Matches the existing Ctrl+C handler pattern. PR
  by @JonusNattapong.

### Changed

- **SSO Sign-in button hidden on hosted cloud workspaces (gateway
  AND BYOK).** v0.41.0 already short-circuited the secrets-backend
  picker when `THCLAWS_GATEWAY_API_KEY` was set; this release
  generalises the cloud-workspace detection to also cover BYOK pods
  (no gateway env, provider keys injected directly). Trigger is now
  `THCLAWS_WORKSPACE_ID` (set by the K8sProvisioner on every cloud
  pod regardless of routing). `ipc.rs::secrets_backend_get` returns
  the sentinel `"hosted"` in both cases, and the desktop frontend
  skips the navbar `<LoginButton/>` so visitors aren't asked to do
  a second OAuth flow inside a workspace they already authenticated
  into at the routing layer. Local desktop installs keep the button.

## [0.41.0] — 2026-06-08

Three issue-driven fixes from the public tracker + two cloud-only
quality-of-life follow-ups that ride along on the same release tag.

### Added

- **Sidebar is now draggable
  ([#150](https://github.com/thClaws/thClaws/issues/150)).** The
  webapp sidebar shipped at a fixed 192px (`w-48`) — too narrow to
  read dated model ids like `claude-sonnet-4-6` vs
  `claude-sonnet-4-5` or longer session titles. A 3px gutter on the
  right edge is now drag-to-resize (clamped to 160–480px, persisted
  in `localStorage` as `thclaws_sidebar_width`). Double-click the
  gutter to reset to the original 192px default. Existing users see
  no shift until they drag. Thanks to @Mayth01 for the report.
- **Model picker hides unpriced rows.** The catalogue
  (`model_catalogue.json`) is now the single source of truth for
  what users can pick — `ModelEntry::is_listable(provider)` filters
  every UI listing (GUI picker, `/model` REPL command,
  `/v1/models` HTTP endpoint) to models that either have a
  published price, are flagged `free` / `tier_billed`, or live on a
  local provider (`ollama` / `ollama-anthropic` / `lmstudio`).
  Prevents bill shock when a backend gateway rejects an unpriced
  model mid-stream.

### Fixed

- **Azure AI Foundry routes by model family
  ([#149](https://github.com/thClaws/thClaws/issues/149)).**
  `build_provider` for `ProviderKind::AzureAIFoundry` always
  pointed at `/anthropic/v1/messages`, so `azure/gpt-*` and other
  OpenAI-protocol Foundry deployments failed with schema errors.
  Inspect the model id after stripping the `azure/` prefix: `claude`
  in the name → unchanged Anthropic protocol; anything else →
  OpenAIProvider on `/openai/v1/chat/completions` with `azure/`
  stripped before reaching the upstream. Single user-facing prefix
  kept, back-compatible for every existing Claude-on-Foundry user.
  Thanks to @thayadev for the report + the drop-in patch.
- **`--serve` aborts on panic so the listening port is released
  ([#151](https://github.com/thClaws/thClaws/issues/151)).** Before
  this fix, a panic in any tokio task (e.g. the UTF-8 char-boundary
  case from #148 on long Thai responses) only unwound that task.
  The runtime stayed alive, port 8443 stayed bound, and systemd's
  `Restart=on-failure` couldn't recover the port without manual
  `kill -9`. The `--serve` dispatch now installs a panic hook that
  chains the default traceback to stderr then `process::abort()`s —
  the OS releases the socket immediately and systemd restarts on a
  clean port. CLI / GUI / print modes are unaffected. The
  underlying char-boundary panic itself shipped in v0.39.0 (#148);
  v0.41.0 makes any future panic in `--serve` fail clean. Thanks
  to @Mayth01 for the durability observation.
- **Gateway-routed workspaces no longer pop the "where to save API
  keys?" picker.** On hosted thClaws.cloud workspaces with
  `THCLAWS_GATEWAY_API_KEY` set, the engine never touches the OS
  keychain or `.env` — every provider call routes through the
  gateway with its own key. `ipc.rs::secrets_backend_get` now
  returns the sentinel `"gateway"` when that env is set, and the
  frontend treats it as already-chosen so the first-launch dialog
  doesn't block the agent UI.

## [0.39.0] — 2026-06-06

UTF-8 char-boundary fix in the agent turn driver + defense-in-depth
session flush on panic.

### Fixed

- **`progress_buf.drain` panic on multi-byte UTF-8 text
  ([#148](https://github.com/thClaws/thClaws/issues/148)).**
  `drive_turn_stream`'s progress-line buffer trimmed itself with a
  raw byte offset (`len() - PROGRESS_BUF_CAP/2`) that could land
  mid-codepoint when the model streamed Thai / CJK / emoji text
  past 4096 bytes. `String::drain` then tripped its
  `is_char_boundary(end)` assertion and the whole turn panicked.
  Worse, the panic killed the future before the `Done` arm ran
  `save_history`, so the in-progress turn — sometimes the whole
  session — disappeared on restart. Snap the offset via
  `str::floor_char_boundary` (stable 1.79+) before draining; the
  trim is now safe regardless of what Unicode the model emits.
  Thanks to @sc28249782 for the spot-on bug report including a
  minimal repro and the exact fix.

### Added

- **`drive_turn_stream` catches panics + flushes the session.** As
  defense in depth against any future panic in the event loop, the
  renamed `drive_turn_stream_inner` now runs inside
  `AssertUnwindSafe(...).catch_unwind().await`. On panic the wrapper
  logs the cause to the lead-log, surfaces `ErrorText` to the user,
  calls `save_history`, refreshes `SessionListRefresh`, emits
  `TurnDone` (so the busy spinner clears), marks the lead mailbox
  idle, then `resume_unwind`s. No public API change.

## [0.34.0] — 2026-06-04

Live-sync fix for the KMS browser sidebar.

### Fixed

- **KMS browser sidebar refetches pages on `kms_update` broadcast.** The
  sidebar now refreshes its page list when KMS updates are broadcast,
  keeping the view synchronized with the current state.

## [0.33.0] — 2026-06-04

Cloud GUI-shell-over-HTTP plus a wave of agent/auth/catalogue hardening.

### Added

- **GUI shell over HTTP + full-screen mode for `cloud serve`.** The
  PTY-backed Shell surface is now reachable over the cloud serve HTTP
  transport, with a full-screen layout mode.

### Fixed

- **Strip orphan `tool_use`/`tool_result` blocks before the provider
  call** ([#144](https://github.com/thClaws/thClaws/issues/144)). A
  dangling tool block left in the transcript (e.g. after an interrupted
  turn) could make providers reject the next request; orphans are now
  pruned before send.
- **Strip wrapping quotes from pasted API keys**
  ([#145](https://github.com/thClaws/thClaws/issues/145)). Keys pasted
  with surrounding `"`/`'` quotes are now unwrapped, with a live
  provider-auth integration suite added as a regression net.
- **De-duplicate `ReloadConfig`** — the settings file-watcher and the
  `/model` write each fired a reload; the duplicate is now coalesced.

### Changed

- **Catalogue cleanup** — pruned dead OpenRouter, DashScope, NVIDIA, and
  MiniMax entries.

## [0.32.2] — 2026-06-03

Patch release — `.thclaws/settings.json` changes now apply without a
restart.

### Fixed

- **Hot-reload of `.thclaws/settings.json`.** Editing settings (e.g.
  flipping `shellTabEnabled` or `teamEnabled`) previously required a full
  process restart because `ProjectConfig::load()` ran once at boot. A
  `notify-debouncer-mini` file watcher (`spawn_settings_watcher`) now
  watches `.thclaws/` non-recursively, debounces at 500 ms, and fires
  `ShellInput::ReloadConfig`. Spawned from `spawn_with_roots` so every
  startup gets it (desktop GUI, CLI REPL, `--serve` pod); idempotent, so
  the picker's own writes re-fire as a no-op.

## [0.32.1] — 2026-06-03

Patch release — cloud heartbeat and a richer engine image.

### Added

- **`thclaws --serve` cloud heartbeat.** Inside a thclaws.cloud
  workspace pod (`THCLAWS_CLOUD_URL`/`_TOKEN`/`WORKSPACE_ID` set), a
  background task pings the control plane's `…/keepalive` every 60 s
  while at least one browser WebSocket is connected — closing the
  idle-reaper edge case where the user closes the dashboard tab but keeps
  the workspace open. No-ops outside cloud; local CLI/desktop unaffected.
- **Engine image bundles ffmpeg + Playwright + Python + Node.** The cloud
  workspace container now bakes in `ffmpeg`, `python3`/`pip`/`venv`,
  `nodejs`/`npm`, and `playwright install --with-deps chromium` so agents
  doing media work or browser automation work out of the box (~600 MB →
  ~1 GB; shared per node via overlayfs).

### Changed

- README hero now an animated carousel (Chat / Terminal / Claude Code);
  June 15 framing corrected to "unbundle, not discontinue".

## [0.32.0] — 2026-06-03

A PTY-backed **Shell** tab — run Claude Code inside thClaws under your
own subscription, ahead of Anthropic's June 15 Agent-SDK unbundling.

### Added

- **GUI Shell tab (PTY-backed).** Spawns `$SHELL` (`/bin/sh` /
  `powershell.exe` fallback) under a real pseudo-tty piped through
  xterm.js — distinct from the agent-rendered `Terminal` tab. Lets you
  run **Claude Code** directly inside thClaws under your normal Claude
  subscription, no third-party API surface. Because `.thclaws/` and
  `.claude/` layouts are compatible, skills / MCP servers / agent
  definitions are shared between the two front-ends. Opt-in via
  `shellTabEnabled: true` (default off — it's an unsandboxed shell with
  no agent-side permission gating). Non-UTF-8 Alt-escapes survive via
  base64 round-trip; resize propagates via TIOCSWINSZ.
- **Files-tab dotfile toggle** — an eye icon reveals `.thclaws/`,
  `.claude/`, `.env`, etc. (off by default) for editing shared config
  inside the GUI.
- **Workflow ergonomics** — `thclaws.include("./helpers.js")` for
  cross-script reuse (traversal-rejected), `thclaws.subagent({prompt,
  agent})` for per-call subagent definitions, and `/workflow exec <path>`
  to run a pre-authored script mid-session.

### Changed

- The previous iframe-shells "Shell" tab is renamed **`UI`**, freeing
  "Shell" for the PTY tab (functionality unchanged).
- **Config parse failures are no longer silent** — a malformed
  `.thclaws/settings.json` now emits a stderr warning with file path and
  serde's line/column hint instead of defaulting every flag off quietly.

## [0.31.0] — 2026-06-03

### Fixed

- **Switching model preserves the conversation**
  ([#142](https://github.com/thClaws/thClaws/issues/142)). The old "new
  session per provider switch" rule is retired — the JSONL transcript is
  canonical and each provider translates it per turn.

### Added

- `CONTRIBUTORS.md` crediting community contributors.

## [0.30.0] — 2026-06-02

### Changed

- **MiniMax default model updated to M3**
  ([#140](https://github.com/thClaws/thClaws/pull/140),
  [@modtanoii](https://github.com/modtanoii)), using canonical casing
  `MiniMax-M3` to match the upstream API.

### Fixed

- **`split_shell_segments` uses `char_indices`**
  ([#141](https://github.com/thClaws/thClaws/issues/141)) — fixes a
  byte-vs-char boundary panic on multibyte input.

## [0.29.0] — 2026-06-02

### Added

- **GUI Shell as the primary interface — Tier 1–3 MVP** (dev-plan/39).
- **Appendix A — providers / models / prices** in the docs, plus **+42
  new models** in the catalogue.

### Fixed

- Backfilled context size on 32 newly-added models and missing pricing on
  `gemini-3.1-flash-lite`.
- **Cap spinner line width to terminal columns**
  ([#139](https://github.com/thClaws/thClaws/pull/139),
  [@gobikom](https://github.com/gobikom)).
- Chart review feedback from
  [#135](https://github.com/thClaws/thClaws/pull/135) addressed
  ([#137](https://github.com/thClaws/thClaws/pull/137)).

## [0.28.0] — 2026-06-01

### Added

- **Helm chart for self-hosted Kubernetes deployment**
  ([#135](https://github.com/thClaws/thClaws/pull/135),
  [@modtanoii](https://github.com/modtanoii)).
- **thClaws.cloud catalog client + agent identity in settings.**
- thClaws.cloud chapter in the user manual + technical manual.

## [0.27.0] — 2026-05-31

### Fixed

- **Clamp `last_saved_count` in `sync()` to prevent a panic after
  `/clear`** ([#134](https://github.com/thClaws/thClaws/pull/134),
  [@gobikom](https://github.com/gobikom)); the CLI REPL also rotates the
  session on `/clear`.

### Added

- REPL refreshes the system prompt on mid-session mutators.
- Subagent factory tracks live state, plus a GUI Shells authoring guide.

## [0.26.0] — 2026-05-31

### Added

- **BM25 `KmsSearch` + native Thai segmenter** (dev-plan/36) — full-text
  knowledge-store search with Thai word segmentation.

## [0.25.0] — 2026-05-31

A follow-up wave centred on three audits + fixes: prompt-builder
unification, tool/MCP registration parity across all four surfaces, and
surfacing collaboration primitives to the model.

### Fixed

- **Skill discovery after cwd change.** `shared_session::ChangeCwd` now
  re-discovers skills via `SkillStore::discover()`; previously the GUI's
  skill store was populated once at startup, so project-scoped
  `.thclaws/skills/` discovered against the launch cwd stayed pinned and
  `/<skill-name>` was reported as an unknown command.
- **Tool + MCP registration parity (5 fixes).** A user-set WebSearch
  engine, Task tools (TodoWrite + queue), team tools, the always-on skill
  family, and plugin-contributed MCP servers were each registered on only
  some of the four surfaces (CLI REPL, GUI/`--serve`, headless print,
  agent_runtime HTTP); all now register consistently per their gates.

### Added

- **Unified system-prompt builder** — `prompts::build_full_system_prompt`
  is the single source of truth for all four surfaces, which previously
  inlined divergent assembly and received different text. Adds a
  `# MCP server instructions` section (per-server briefings from each
  MCP's `InitializeResult.instructions`, previously captured but
  discarded) and a `# Collaboration primitives` section.
- **Model-callable `WorkflowRun` tool** — the same author + sandbox flow
  as `/workflow run`, so the model can reach for deterministic fan-out on
  its own (requires approval; nested calls rejected). Wired into all four
  surfaces.

## [0.24.0] — 2026-05-30

Two major threads land: GUI Shell Tier 2 and multi-tenant `--serve`.

### Added

- **GUI Shell — Tier 2** (dev-plan/33). A third tab mode picks a
  single-HTML or html+js+css folder as the agent's frontend, served at
  `/t/<token>/` behind a persisted-token URL with no direct browser
  access to the shell folder. Shell discovery layers built-in → user →
  project (last wins); a sandboxed bridge runtime (`thclaws.run` /
  `thclaws.on` / `thclaws.storage`) is injected at serve time. Per-project
  adapter configs now read from `./.thclaws/<adapter>.json`.
- **Multi-tenant `--serve` — Tier 1** (dev-plan/35). One pod hosts N
  users with HMAC-SHA256 signed routing from a trusted layer, per-user
  `SharedSessionHandle`s with isolated on-disk state under
  `.thclaws/users/<id>/`, LRU + idle eviction, a file-asset URL gate
  (HMAC + path-prefix), and a `MeteringSink` trait (HTTP/stdout/noop).
  Single-tenant defaults unchanged. Covered by restart-recovery,
  50-user-concurrency no-cross-leakage, and HMAC-handshake tests.

## [0.23.0] — 2026-05-29

### Added

- **Dynamic workflows** (dev-plan/32) — Tier 1 `/workflow run` plus the
  full Tier 2 + Tier 3 workflow surface for deterministic multi-agent
  orchestration.
- **Self-contained `/quiz`** embedded in thClaws
  ([#132](https://github.com/thClaws/thClaws/pull/132)), dropping the
  external gamedev MCP dependency.

## [0.22.0] — 2026-05-28

### Added

- **Tool progress visibility**
  ([#130](https://github.com/thClaws/thClaws/pull/130),
  [@gobikom](https://github.com/gobikom)) — contextual tool labels (Bash
  command, Read path, Grep pattern, …), a Braille spinner with elapsed
  timer, heartbeat lines for long-running tools, and a ✓/✗ completion
  suffix with duration. A new `tool_display` module centralises
  formatting and redacts secrets (Bearer tokens, `--api-key=`, …) from
  every label.
- **Typed `ProviderEvent::Progress` channel** — spinner state now flows
  separately from `TextDelta`, so animation never leaks into `lead_log`,
  session JSONL, GUI envelopes, or accumulated assistant text. The REPL
  spinner is gated on `IsTerminal` so piped/headless runs stay ANSI-free.

### Changed

- README restructured to attract contributors.

## [0.21.0] — 2026-05-28

### Added

- **Facebook Page Messenger adapter — Tier 1** (dev-plan/31). Chat with
  your thClaws install from a Page inbox. Messenger is webhook-only, so
  the bridge is relay-based (extending the LINE relay with a
  `/messenger/webhook` route + Graph Send API client); the Page Access
  Token and App Secret live on the relay, never on the desktop. Pair a
  Page with a 6-digit code, then drive thClaws from a phone, with
  quick-reply chips as the approval surface for mutating tools.
  - New `crates/core/src/messenger/` module; GUI Messenger Connect modal +
    sidebar pill + boot-time auto-reconnect.
  - Headless via `thclaws --messenger`, plus `thclaws messenger
    status`/`pair` subcommands.
  - `PermissionMode::MessengerGated` (folds with LineGated /
    TelegramGated); 2,000-char chunked output filter reusing the LINE
    ANSI/tool-narration stripper.
  - User manual ch24 (EN + TH) + technical manual cover Meta app setup,
    webhook subscription, pairing flow, and privacy boundary.

  Tier-1 known gaps: single shared session per Page (no per-PSID
  isolation), approval prompts target the most-recent inbound PSID, and
  production beyond app testers needs Meta App Review + Business
  Verification for `pages_messaging`.

## [0.20.0] — 2026-05-26

Telegram channels + forum topics + streaming preview, plus two
community-driven hardening fixes.

### Added

- **Telegram channels + forum-topic routing (Tier 2).** The bot can post
  to a broadcast **channel**, and comments on those posts (which land in
  the channel's linked **discussion group**) reach the agent. Supergroup
  **forum topics** route to different agents: `channels[].topicRouting`
  maps a topic id to an `agentId` (an AgentDef under `.thclaws/agents/`),
  falling back to the channel's default agent. Replies go back into the
  originating topic, with the "General" topic's `message_thread_id=1`
  send quirk handled. A `getChatMember` admin-rights probe returns a
  clear error when the bot isn't an admin that can post. Per-topic
  multi-agent routing is honoured by headless `thclaws --telegram`; the
  GUI runs its single shared session.
- **Telegram streaming preview edits (Tier 3.1).** Opt-in via
  `streamPreview` in the Telegram config: instead of one reply at the end
  of a turn, post a placeholder and **edit it in place** as the agent
  generates (rate-limited to avoid Telegram's same-message edit
  throttling), then swap in the final formatted reply. Headless-only for
  now.

### Fixed

- **Grapheme-aware Backspace in the CLI REPL**
  ([#126](https://github.com/thClaws/thClaws/pull/126),
  [@modtanoii](https://github.com/modtanoii)). Backspace deleted one
  codepoint per press, orphaning Thai/Lao/Hindi/Arabic combining marks
  and splitting emoji ZWJ sequences. It now deletes a whole grapheme
  cluster, via a rustyline `ConditionalEventHandler` + `unicode-segmentation`
  (rather than vendoring rustyline).

- **Shell-aware team bash seatbelts**
  ([#125](https://github.com/thClaws/thClaws/issues/125),
  [@ultramcu](https://github.com/ultramcu)). The team-lead / teammate
  destructive-command guards matched by substring and were defeated by
  shell quoting (`r''m -rf`, `$(printf rm)`, `${x:-rm}`, backticks,
  `{rm,-rf,..}`, `IFS`-splicing, `eval $'\x72\x6d'`, arg-order swap,
  quoted verb, line-continuation, wrapper prefixes) — letting an LLM
  lead/teammate in `--accept-all` mode slip a destructive command past
  the seatbelt. They now tokenize via `shell_words` (resolving quotes /
  order / wrappers, recursing into `eval` / `sh -c` / `bash -c`) and
  refuse obfuscated forms carrying a destructive signal; the substring
  guard remains as a fallback.

### Default model — no change

Default stays `claude-sonnet-4-6`.

## [0.19.0] — 2026-05-25

Telegram bot adapter — chat with your local thClaws agent from Telegram.

### Added

- **Telegram bot adapter (Tier 1).** Create a bot with `@BotFather`,
  connect it from the desktop (Settings → **Telegram Connect**) or run it
  headless with `thclaws --telegram`, and DM your local agent from
  anywhere. The agent and all its tools stay on your machine; Telegram is
  only the chat surface, and there is **no relay** — thClaws talks to the
  Bot API directly via long-polling (works behind NAT).

  - DM + basic group support; pairing-code onboarding (the owner approves
    new users from the GUI); HTML-formatted replies chunked to Telegram's
    4096-character message limit.
  - Tool calls that need approval surface as **inline-keyboard buttons**
    (Allow / Always / Deny) via a new `telegramgated` permission mode —
    approve `Bash`/`Edit`/`Write` from your phone.
  - `thclaws telegram status | pair` CLI; env-first token
    (`TELEGRAM_BOT_TOKEN`), `TELEGRAM_OWNER_ID` for instant headless
    allowlisting.
  - Docs: new Chapter 23 in the EN + TH user manuals and
    `telegram-bridge.md` in the technical manual.

### Fixed

- **Agent SDK: avoid `ARG_MAX` on large system prompts**
  ([#124](https://github.com/thClaws/thClaws/pull/124),
  [@gobikom](https://github.com/gobikom)). The Agent SDK provider passed
  the assembled system prompt as a single `--system-prompt` CLI argument;
  with MCP tools + CLAUDE.md + skills + memory + KMS it can exceed Linux's
  128 KB `MAX_ARG_STRLEN` → `spawn claude: Argument list too long`,
  blocking `agent/claude-*` models in `--cli` when MCP servers are
  registered. The prompt is now written to a temp file and passed via
  `--system-prompt-file`.

### Default model — no change

Default stays `claude-sonnet-4-6`.

## [0.18.0] — 2026-05-24

One-shot schedules ("run once in 15 minutes / tomorrow at 9am"), plus
two community fixes.

### Added

- **One-shot / relative-delay schedules**
  ([#122](https://github.com/thClaws/thClaws/issues/122),
  design by [@ultramcu](https://github.com/ultramcu)). Schedules can now
  run **once** at a future time or after a relative delay, alongside the
  existing recurring cron jobs:

  ```sh
  thclaws schedule add report --at "2026-05-24T15:30:00Z" --prompt "…"
  thclaws schedule add check  --in 15m                    --prompt "…"
  ```

  `--in` accepts `s`/`m`/`h`/`d` (and a bare integer as seconds);
  `--at` takes an RFC 3339 timestamp. Both are mutually exclusive with
  `--cron`. A one-shot fires once, then auto-disables. **Catch-up by
  design:** a fire time already in the past when the scheduler ticks
  (e.g. the daemon was down over the slot) runs immediately rather than
  being lost — the footgun of hand-writing a cron for a single minute,
  where a missed slot silently waits a year. `schedule list` shows
  `once@<time> (pending|fired)`; the new on-disk `runAt` field is
  optional, so existing `schedules.json` files stay compatible.

### Fixed

- **Edit: reject an empty `old_string`**
  ([#121](https://github.com/thClaws/thClaws/pull/121),
  [@ultramcu](https://github.com/ultramcu)). An empty `old_string`
  matches between every character, so with `replace_all` it would inject
  the replacement throughout the file and corrupt it. The Edit tool now
  rejects it up front.

- **ChatGptCodex credentials detected from the auth file**
  ([#123](https://github.com/thClaws/thClaws/pull/123),
  [@gobikom](https://github.com/gobikom)). `kind_has_credentials()` only
  probed env vars, but ChatGptCodex (ChatGPT subscription) authenticates
  via a file-based OAuth token — so it was wrongly reported as having no
  credentials, and interactive `--cli` / GUI / `--serve` triggered the
  model-fallback path and overwrote `settings.json`. It now resolves the
  Codex auth store (honoring token expiry), and the shared-session
  worker delegates to the same canonical check so all surfaces agree.

### Default model — no change

Default stays `claude-sonnet-4-6`.

## [0.17.1] — 2026-05-24

KMS + Files management in the GUI, a LINE reconnect fix, and a clearer
sandbox boundary message.

### Added

- **KMS sidebar create / rename / delete / edit.** The `+` buttons now
  open proper modals (the old `window.prompt`/`confirm` silently failed
  inside the wry webview): create a new KMS base (name + project/user
  scope), and create a new blank page (title / topic / category / tags)
  from the per-KMS browser panel. Right-click a page row to **Rename…**
  (moves the file and rewrites inbound links + the index) or
  **Delete…**. Edit the page you're viewing — a pencil opens the body
  in the TipTap editor plus a modal for the raw YAML frontmatter; Save
  writes it back.
- **Files tab create file / folder.** Right-click the explorer (or the
  new FilePlus / FolderPlus header buttons) for **New file…** /
  **New folder…**, created in the current directory via a name modal.
  Sandbox-checked; refuses to clobber an existing path. The explorer
  header now shows a compact `../<last>` path (full path on hover)
  since the viewer navbar already carries the full path.

### Fixed

- **LINE: reconnect storm after a clean websocket close**
  ([#120](https://github.com/thClaws/thClaws/pull/120),
  [@ultramcu](https://github.com/ultramcu)). `LineClient::run` reset
  backoff and reconnected immediately on a clean close; a relay that
  closes cleanly on every connect spun an unthrottled connect/close
  loop. Adds a cancel-aware 1s pause mirroring the error path (shutdown
  still returns `Cancelled` promptly).

- **Clearer "outside the workspace" sandbox message**
  ([#119](https://github.com/thClaws/thClaws/issues/119),
  [@ruzerix](https://github.com/ruzerix)). When a path resolves outside
  the workspace root, `Sandbox` now states plainly that this is a
  workspace-path boundary, **not** a permission/approval issue
  (approving a tool doesn't widen it). #119 turned out not to be a bug:
  a small model fabricated an out-of-workspace absolute interpreter
  path, the command failed as an ordinary shell error, and the model
  paraphrased it as "rejected by the security policy." The Bash tool
  description now steers models to invoke interpreters via PATH
  (e.g. `python script.py`) rather than guessing absolute paths.

### Default model — no change

Default stays `claude-sonnet-4-6`.

## [0.17.0] — 2026-05-24

Two contributor-driven fixes: accurate Anthropic token/cost accounting,
and a remote-MCP `/mcp add` that no longer hangs (with API-key header
support).

### Added

- **`--header` on `/mcp add`** (part of
  [#118](https://github.com/thClaws/thClaws/pull/118)).
  `/mcp add <name> <url> --header "Key: Value"` — repeatable, `-H`
  alias. Values support `${VAR}` interpolation resolved from the
  environment at connect time, so an API key lives in your shell /
  `.env` rather than plaintext in `mcp.json`:
  ```
  /mcp add financial-datasets https://mcp.financialdatasets.ai/api --header "X-API-KEY: ${FD_KEY}"
  ```

### Fixed

- **Anthropic token usage + prompt-cache accounting**
  ([#115](https://github.com/thClaws/thClaws/pull/115),
  [@ultramcu](https://github.com/ultramcu)). The streaming parser read
  usage only from `message_delta` (which carries just `output_tokens`)
  and dropped `message_start.message.usage`, so every Anthropic turn
  reported `input_tokens = 0` and no cache stats — making `/cost` and
  the Cardputer cost display undercount the flagship provider. Now
  merges `message_start` usage into the terminal result (terminal
  `output_tokens` wins; cache fields preserved).

- **Remote MCP `/mcp add` no longer hangs; supports API-key auth**
  ([#114](https://github.com/thClaws/thClaws/issues/114),
  [@ultramcu](https://github.com/ultramcu);
  [#118](https://github.com/thClaws/thClaws/pull/118)). Adding an
  OAuth-gated remote server (e.g. financial-datasets' root URL) froze
  `/mcp add` for up to 5 minutes: the command ran the full connect
  inline, hit a 401, and blocked on the OAuth browser callback. Four
  fixes:
  - `--header` lets you use the API-key endpoint (`/api` + `X-API-KEY`)
    and skip OAuth entirely (see Added).
  - The auth probe and `oauth::discover` now have hard timeouts (15s
    request / 10s connect) so a stalled server can't hang the command
    or a startup spawn indefinitely.
  - `/mcp add` connects **non-interactively**: a server that needs
    OAuth returns "run `/mcp reauth <name>`" instead of blocking on a
    browser callback. The guard covers both the upfront probe and the
    bridge's `initialize`-time 401. Startup / `/mcp reauth` stay
    interactive (browser flow runs in the background as before).

### Default model — no change

Default stays `claude-sonnet-4-6`.

## [0.16.1] — 2026-05-24

Hotfix. macOS startup crash for every GUI / `--serve` user.

### Fixed

- **macOS: GUI / `--serve` build crashed on startup (TCC / Bluetooth SIGABRT)**
  ([#116](https://github.com/thClaws/thClaws/issues/116),
  [@ultramcu](https://github.com/ultramcu);
  [#117](https://github.com/thClaws/thClaws/pull/117)).
  The `cost_bridge` feature (Cardputer cost display, added in v0.15.0)
  was enabled by default and started a Bluetooth LE scan on every
  launch via `cost_bridge::spawn()` → `adapter.start_scan()`. On
  macOS, any binary without an `NSBluetoothAlwaysUsageDescription`
  `Info.plist` — every `cargo build` and every GitHub release archive
  (none are `.app` bundles) — is killed by **TCC** with a hard
  **SIGABRT** ~1–3s after startup, before serving any request. It also
  popped a Bluetooth permission prompt for the ~99% of users who don't
  own a thClaws-Cost Cardputer.
  - Fix: `cost_bridge` is now **opt-in** (`default = []`). A stock
    build never links `btleplug` or starts the BLE scan. Cardputer
    users build with `--features cost_bridge`.
  - No code changes — the call sites were already
    `#[cfg(feature = "cost_bridge")]`-gated.
  - **Affected releases v0.15.0 and v0.16.0**: macOS users on those
    versions should upgrade to v0.16.1, or run with
    `cargo run --no-default-features --features gui` as a workaround.

## [0.16.0] — 2026-05-23

Four user-facing fixes — three issue-driven, plus a Files-tab polish item
caught while drafting a deck.

### Fixed

- **Windows: GUI launch no longer blocks the cmd.exe / PowerShell prompt**
  ([#109](https://github.com/thClaws/thClaws/issues/109),
  [@jubbyy](https://github.com/jubbyy);
  [#111](https://github.com/thClaws/thClaws/pull/111)).
  Typing `thclaws.exe` from a shell on Windows 11 used to leave the
  prompt waiting until the GUI window closed. Root cause: PR #60 (May
  2026) deliberately built the binary as the **console subsystem** so
  `--cli`'s rustyline gets working stdio — the side effect was that
  cmd.exe / PowerShell `WaitForSingleObject` on every console-subsystem
  child, and `FreeConsole()` in the child can't undo that. Fix: at GUI
  dispatch entry on Windows, respawn `current_exe()` with
  `THCLAWS_GUI_DETACHED=1` and the `DETACHED_PROCESS` creation flag,
  then `exit(0)` the parent. The detached child runs the GUI in-process
  and survives parent / terminal closure; the parent exits in
  microseconds so cmd's wait returns. Placed before the in-process
  scheduler and `/v1` loopback bind so neither runs in the doomed
  parent (no port-bind race on 18443). Spawn failure (antivirus
  quarantine, ENOMEM, etc.) falls through to the in-process GUI run.
  No-op on macOS / Linux.

- **Agent: `max_tokens` escalation retry no longer rejected by claude-opus-4-7+**
  ([#112](https://github.com/thClaws/thClaws/pull/112),
  [@siharat-th](https://github.com/siharat-th)).
  When the model hit `stop_reason=max_tokens` with no tool uses, the
  loop escalated `max_tokens` to 64000 and retried. The partial
  assistant message was already pushed to history, so the next
  `provider.stream` call's messages ended with `role=assistant` —
  which claude-opus-4-7+ rejects ("This model does not support
  assistant message prefill. The conversation must end with a user
  message."), failing the entire retry. Fix: pop the trailing
  assistant (guarded on `role == Assistant` so an empty assistant
  push is a no-op) before `continue`. The retry now sees a clean
  conversation ending in `role=user` and the model produces a
  complete response under the larger budget.

- **CLI: `--model` flag now reaches GUI and `--serve` modes**
  ([#110](https://github.com/thClaws/thClaws/pull/110),
  [@dome](https://github.com/dome) — original diagnosis;
  [#113](https://github.com/thClaws/thClaws/pull/113)).
  `thclaws --model X` was applied only by the CLI/REPL branch in
  `app.rs::main`, so the GUI and `--serve` paths silently ignored
  it. Fix: route `--model` through a process-global override that
  `AppConfig::load` applies last, after the project overlay — every
  dispatch surface (CLI, GUI, `--serve`, `--serve --gui`) now honors
  the flag without per-mode override plumbing. The GUI's
  auto-fallback path clears the override after switching to a
  working provider so a broken `--model` choice doesn't re-pin on
  every reload. Closes #110.

- **Files preview: relative `![alt](img/foo.png)` in markdown now renders.**
  Comrak emits `<img src="img/foo.png">` verbatim, and the iframe's
  `srcDoc` base URL is opaque, so relative paths had no directory to
  resolve against and failed silently. Fix: inject a
  `<base href="${origin}/file-asset/<dir>/">` into the rendered HTML
  before `srcDoc` so relative refs resolve via the same
  `/file-asset/` handler the `.html` branch already uses — same
  sandbox check, no backend changes.

### Added

- **`--set-model VALUE` flag**
  ([#113](https://github.com/thClaws/thClaws/pull/113)).
  Persists a model to `.thclaws/settings.json` as the project
  default *and* uses it for the current run. Kept separate from
  `--model` (one-shot, in-memory) on purpose: a scripted
  `thclaws --print --model gpt-4-mini "quick"` shouldn't silently
  rewrite the default the user keeps for interactive work.
  Distinguishes "file missing" (safe to create — falls back to
  `ProjectConfig::load` so `.claude/settings.json` migrations
  preserve existing settings) from "file exists but unreadable"
  (bail with a clear error rather than silently nuking siblings
  like `maxTokens` / `allowedTools` / `kms.active` with a
  defaults-everywhere `ProjectConfig`). Save errors surface on
  stderr; success prints a green confirmation with the resolved
  path.

### Default model — no change

Default stays `claude-sonnet-4-6`.

## [0.6.2] — 2026-04-27

Patch release. Two open-issue fixes plus a routine catalogue refresh.

### Fixed

- **Terminal slash-command popup cursor desync** ([#31](https://github.com/thClaws/thClaws/issues/31),
  [@mrpokx5](https://github.com/mrpokx5)). After accepting a command via Tab
  or mouse click, the JS-side `cursorPos` stayed at its pre-accept value
  while the visible terminal cursor jumped to the end of the rewritten
  command. Subsequent keystrokes used the stale `cursorPos` to slice +
  splice `lineBuffer`, mangling the command name (the user's reported
  "selected but can do nothing" + "cursor misplaced on mouse click").
  Fix: assign `cursorPos = next.length;` after the buffer rewrite.
  Single line, three accept paths covered (Tab key, Enter when name
  still being composed, popup mouse onClick).

- **Retired Gemini models in catalogue** ([#32](https://github.com/thClaws/thClaws/issues/32),
  [@jubbyy](https://github.com/jubbyy)). Reporter hit 404 on
  `gemini-2.0-flash`. Cross-checked against
  [Google's official deprecations page](https://ai.google.dev/gemini-api/docs/deprecations) —
  the model is in "existing-customer-only" since 2026-03-06 with hard
  shutdown 2026-06-01. Removed 7 retired rows from the catalogue:
  - `gemini-1.5-flash`, `gemini-1.5-pro` (1.x family fully shut down 2025)
  - `gemini-2.0-flash`, `-001`, `-lite`, `-lite-001` (shutdown 2026-06-01)
  - `gemini-3-pro-preview` (already shut down 2026-03-09; replaced by `gemini-3.1-pro-preview`)

  Added `is_retired_gemini` filter in `catalogue-seed` so future
  `make catalogue` runs won't re-add them even though Google's upstream
  `/v1beta/models` still lists them for backward-compat. Verified the
  filter held against a live refresh — Gemini stayed at 10 rows. Comment
  in the filter points at Google's deprecations page so the next
  maintainer knows where to update.

### Catalogue

Routine refresh added 6 new model rows:

- **OpenRouter** — 5 new Qwen entries: `qwen/qwen3.5-plus-20260420`,
  `qwen/qwen3.6-{27b,35b-a3b,flash,max-preview}`.
- **Ollama Cloud** — `ollama-cloud/deepseek-v4-pro`.

Catalogue total now 589 rows (down 1 from v0.6.1's 590, net of the 7
retirements minus 6 additions).

### Default model — no change

The default Gemini model stays at `gemini-2.5-flash`. Considered switching
to Google's `gemini-flash-latest` rolling alias for auto-tracking, but
rejected — `-latest` could promote a higher-tier model into the alias
without warning, surprising users with unexpected cost. Convention
matches Anthropic / OpenAI defaults (pinned versioned IDs). Next bump
deadline: **2026-06-17** when `gemini-2.5-flash` retires per Google's
schedule. Comment near the default points at the deprecations page so
the next maintainer knows when to bump.

## [0.6.1] — 2026-04-27

Patch release. Three community PRs landed in quick succession after
v0.6.0 — a real cost optimization, a contributor-experience improvement,
and a new provider variant. All three fully tested, no breaking changes.

### Added — `OpenAICompat` provider ([#35](https://github.com/thClaws/thClaws/pull/35), [@SalmonRK](https://github.com/SalmonRK))

A first-class slot for generic OpenAI-compatible HTTP endpoints — LLM
gateways like LiteLLM, Portkey, Helicone, internal corporate proxies,
self-hosted inference servers (vLLM, text-generation-inference,
lm-deploy), and any other service that speaks OpenAI's
`/v1/chat/completions` wire format with a Bearer token.

Mirrors the existing `LMStudio` / `DashScope` / `ZAi` /
`AzureAIFoundry` template — a configurable base URL (`OPENAI_COMPAT_BASE_URL`
or Settings UI), Bearer token from `OPENAI_COMPAT_API_KEY`, and a
`oai/<id>` model prefix that is stripped before the request reaches
the upstream. Real OpenAI (`OPENAI_API_KEY` + `gpt-*` / `o*` models)
is unaffected — there is no env-var collision and no slot shadowing.

Usage:

```sh
# .env or shell
export OPENAI_COMPAT_BASE_URL=http://localhost:8000/v1
export OPENAI_COMPAT_API_KEY=...

# in REPL or via --model flag
/model oai/<upstream-model-id>
```

The `oai/` prefix is stripped before the wire payload, so an upstream
model named `meta-llama/Llama-3.1-70B-Instruct` is reached via
`/model oai/meta-llama/Llama-3.1-70B-Instruct`.

### Added — Anthropic third cache breakpoint ([#33](https://github.com/thClaws/thClaws/pull/33), [@chawasit](https://github.com/chawasit))

Adds a `cache_control: ephemeral` marker on the last content block of
the second-to-last message in `AnthropicProvider::build_body`, turning
the rolling conversation history into a cached prefix on subsequent
turns. The newest message stays uncached (it's the live user turn);
the one before it is byte-stable across the next call and becomes the
cache anchor.

Anthropic supports up to 4 `cache_control` markers per request. Before
this change we used 2 (system prompt + last tool definition); both
cached *fixed-size* blocks. The growing conversation history was
re-tokenized in full on every turn even though everything except the
newest user message was byte-stable across the next call.

Approximate input-cost reductions on Sonnet 4.6 vs. the prior
2-breakpoint setup:

| Session length × shape | Saving vs. 2 breakpoints |
|---|---|
| 10 turns, normal coding | ~46% |
| 10 turns, tool-heavy | ~54% |
| 30 turns, normal coding | ~74% |

Break-even is one cache hit: the 25% write surcharge is recovered
the next time the cached prefix is reused at 90% off. Anthropic's
1024-token minimum-cacheable-prefix floor is enforced server-side;
the client adds the marker only when the history has at least 3
messages (a soft-skip so the breakpoint slot isn't burned on
sub-1024-token histories that almost certainly won't qualify).

Three new tests cover the positive case, the short-history guard,
and a byte-stability invariant guarding against silent cache busts
from non-deterministic field ordering.

### Added — `scripts/build.{sh,ps1}` build helpers ([#34](https://github.com/thClaws/thClaws/pull/34), [@chawasit](https://github.com/chawasit))

One-shot cross-platform build helpers. Default behavior: build the
frontend (`pnpm install` + `pnpm build`), then `cargo build --features
gui`. The Rust GUI build embeds `frontend/dist/index.html` at compile
time, so a bare `cargo build --features gui` without a prior frontend
build fails with a confusing missing-file error from `include_str!`.
The helpers enforce the order and surface a clear "you forgot to build
the frontend" message instead.

| `bash` | `PowerShell` | Effect |
|---|---|---|
| `scripts/build.sh` | `scripts/build.ps1` | debug build (frontend + cargo) |
| `--release` | `-Release` | release profile |
| `--no-frontend` | `-NoFrontend` | skip pnpm steps; assume `frontend/dist` exists |
| `--check` | `-Check` | full verification suite (`cargo fmt --check`, `clippy -- -D warnings`, `pnpm tsc --noEmit`, `cargo test`) |

Includes a `.gitattributes` that pins `*.sh` to LF and PowerShell /
batch files to CRLF so the bash script stays executable on Linux/macOS
even when the repo is checked out on Windows with `core.autocrlf=true`.
Without this, every Windows checkout would mangle the bash script's
shebang line and break it on POSIX hosts.

### Internal cleanup

- Two `clippy` warnings in `crates/core/build.rs` cleaned up
  (`collapsible_str_replace`, `manual_div_ceil`) — these were
  pre-existing from the v0.5.0 Phase 0 EE work and were noted in
  the PR descriptions of #33 and #34. `cargo clippy --fix` also
  applied 8 mechanical fixes across `repl.rs`, `skills.rs`,
  `providers/mod.rs`, `model_catalogue.rs`, `sso/discovery.rs`, and
  `bin/catalogue_seed.rs`. **505 lib tests pass.**

## [0.6.0] — 2026-04-27

Minor release — Enterprise Edition Phase 4 (OIDC SSO) + admin
deployment UX. Open-core users see zero behavior change; every
feature below is inert unless a verified org policy with
`policies.sso.enabled` is loaded.

### Added — OIDC SSO (Phase 4)

- **Browser-driven OIDC authorization-code + PKCE flow.** Works
  against any standards-compliant IdP — Okta, Azure AD / Entra ID,
  Auth0, Keycloak, Google Workspace, AWS Cognito — selected by
  `policies.sso.issuer_url` in the active org policy. New module
  surface under `crates/core/src/sso/`:
  - `pkce.rs` — RFC 7636 verifier/challenge generator (32-byte
    OS-RNG verifier → SHA-256 → S256 challenge), RFC 7636 Appendix B
    test vector covered.
  - `discovery.rs` — fetches `<issuer>/.well-known/openid-configuration`,
    validates S256 PKCE support, decodes endpoints. One implementation,
    all IdPs.
  - `loopback.rs` — minimal HTTP listener on `127.0.0.1:<random>`
    (~60 lines `std::net`, no extra HTTP-server dep). Reads request
    line, extracts `code`/`state`/`error`, returns a friendly "you can
    close this tab" HTML page, shuts down. 5-minute timeout so a user
    who closes their browser doesn't hang the agent.
  - `storage.rs` — keychain persistence via the existing `secrets`
    module. Cache key is `thclaws-sso-<sha256-of-issuer>` so flipping
    IdPs doesn't pollute new claims with stale ones. Tokens never
    touch disk plaintext.
  - `mod.rs` — public API: `login`, `logout`, `current_session`,
    `current_access_token`, `status`, `decode_id_token_claims`. Token
    exchange via `reqwest`. Background refresh kicked off via
    `tokio::spawn` when within 60s of expiry. CSRF-safe `state` parameter
    refused on mismatch.

- **Slash commands**: `/sso`, `/sso login`, `/sso logout`, `/sso status`.
  Wired in both REPL and GUI dispatch.

- **GUI sidebar Identity section**. Three new IPC handlers
  (`sso_status` / `sso_login` / `sso_logout`) + a React component that
  renders only when the active policy has `sso.enabled`. Shows
  signed-in state with email + token-expiry pill + sign-out link, or
  not-signed-in state with a sign-in button. Open-core deployments
  never see the section at all.

- **Gateway `{{sso_token}}` substitution wired**. The Phase 3 gateway's
  auth-header template now resolves `{{sso_token}}` from the active
  SSO session at request time. Per-user identity flows through to the
  gateway audit log: instead of "device-token-X used claude-sonnet-4-6"
  the audit shows "alice@acme.com used claude-sonnet-4-6". Phase 3
  rendered this placeholder as empty string; v0.6.0 makes it active.

### Added — Policy schema (SsoPolicy fields)

- **`clientSecret`** (inline literal) — for "non-confidential" secrets
  that ship embedded in every binary copy by design (Google's Desktop
  OAuth being the canonical example, with Google's own docs explicitly
  classifying these as not-actually-secret). Recommended for those
  IdPs because it collapses the deploy story to "one signed file =
  one deployment artifact."

- **`clientSecretEnv`** (env var name) — for real confidential
  secrets that should never embed in deployed artifacts. The named env
  var is read at token-exchange time. Deploy via MDM / login script /
  OS keychain alongside the binary, in the same channel as the signed
  policy file.

  Resolution order: `clientSecret` (inline) → `clientSecretEnv` (env
  lookup) → none (PKCE-only public client). Each layer treats blank /
  missing as "not set" so a stray space or a left-over `=""` line in
  `.env` doesn't accidentally authenticate as the empty string.

### Added — Operator workflow (Make targets)

Six new Make targets that drive the EE lifecycle end-to-end:

- `make gen-key` — generates Ed25519 keypair at `thclaws-config/policy.{pub,key}`,
  chmod 600 on Unix, refuses to overwrite without `FORCE=1`.
- `make policy-google` — signed policy template targeting
  `accounts.google.com`, reads `GOOGLE_CLIENT_ID` / optional
  `GOOGLE_CLIENT_SECRET` from `.env`, embeds inline.
- `make policy-okta` — Okta tenant template, reads `OKTA_ISSUER_URL` /
  `OKTA_CLIENT_ID` / optional `OKTA_CLIENT_SECRET`, uses
  `clientSecretEnv` (Okta secrets are real).
- `make policy-azure` — Azure / Entra template, reads `AZURE_TENANT_ID`
  / `AZURE_CLIENT_ID` / optional `AZURE_CLIENT_SECRET`, builds the v2
  issuer URL automatically (`login.microsoftonline.com/<tenant>/v2.0`
  — v1 lacks the OIDC discovery doc).
- `make remove-key` — clears the public key + signed policy from the
  build-pickup path. Leaves the private key alone (admin may want to
  keep signing more policies). Idempotent. Useful for "build a clean
  open-core binary from this same checkout" workflows.
- `make remove-keypair FORCE=1` — destructive wipe of all keypair
  material. Refuses without `FORCE=1` because losing the private key
  means existing signed policies can't be re-signed.

Forward path (open-core → enterprise): `gen-key` → `policy-google` (or
`policy-okta` / `policy-azure`) → `make build`.
Backward path (enterprise → open-core): `remove-key` → `make build`.

### Added — Documentation

- **`docs/enterprise-make.md`** — canonical operator reference for the
  EE lifecycle. Covers prerequisites, target reference, lifecycle
  workflows (initial setup, re-sign, annual rotation, multi-customer
  pipeline, switching IdPs, going back to clean open-core),
  troubleshooting, file layout, and design principles.

- **`ENTERPRISE.md`** updated to reflect Phase 4 shipped: status table
  moved SSO from "Planned for v0.6.0" to "Shipped".

### Caveats

- **Live smoke confirmed against Google Workspace.** Okta and Azure
  templates are unit-tested with synthetic credentials but haven't
  been exercised against a real tenant. Any tenant-specific quirks
  surface in early customer feedback, not in this CHANGELOG.

- **Frontend hardcoded "thClaws" strings** in `App.tsx` /
  `ChatView.tsx` still aren't routed through the branding module —
  same caveat carried over from v0.5.0. Phase 4 covered the GUI
  Identity section but didn't expand the branding-IPC surface.

- **Tool-call audit (WebFetch / WebSearch URLs)** is still not
  gateway-routed. Those are general-purpose web fetches, not LLM
  provider calls, and intentionally bypass the gateway in v0.6.0. An
  admin who wants to gate them would do so at the network firewall
  level. A future sub-policy could add this if customers ask.

## [0.5.0] — 2026-04-27

Minor release. Lands the **Enterprise Edition foundation** (Phases 0–3
of `dev-plan/01-enterprise-edition.md`) — policy infrastructure,
branded builds, plugin/skill/MCP allow-list, and gateway enforcement.

**Open-core users see zero behavior change.** Every feature below is
inert unless an Ed25519-signed organization policy file is present at
`~/.config/thclaws/policy.json` or `/etc/thclaws/policy.json` *and*
verifies against either an embedded public key (enterprise builds) or
one supplied at runtime via env var / conventional file path.

### Added — Enterprise Edition foundation

- **Org policy file format** (Phase 0). New `policy/` module with a
  versioned JSON schema covering four sub-policies (branding, plugins,
  gateway, sso), Ed25519 signature verification using a hand-written
  canonical-JSON serializer (no external `canonical-json` dep), expiry
  checks, and optional `binding.binary_fingerprint` matching to prevent
  lifting a customer's policy onto a non-customer build. Loader searches
  `THCLAWS_POLICY_FILE` → `/etc/thclaws/policy.json` → `~/.config/thclaws/policy.json`.
  Public key sources: compile-time embedded → `THCLAWS_POLICY_PUBLIC_KEY`
  env var → `/etc/thclaws/policy.pub` → `~/.config/thclaws/policy.pub`.
  Open-core release binaries embed no key; enterprise builds bake the
  customer's public key at compile time via `THCLAWS_POLICY_PUBKEY_PATH`.
  Refuses to start on signature failure, expiry, binding mismatch, or
  missing verification key — fail-closed by design.

- **`thclaws-policy-tool` operator CLI** (Phase 0). Subcommands:
  `keygen` (generates Ed25519 keypair, chmods private key 0600 on Unix),
  `sign` (signs a policy JSON file), `verify` (checks signature
  against a public key), `inspect` (pretty-prints policy structure),
  `fingerprint` (computes SHA-256 of a binary for `binding`). Signing
  logic lives **only** in this tool — main runtime has zero signing
  code, so a leaked source tree isn't a key-compromise vector.

- **Branding config** (Phase 1). New `branding` module reads
  `policies.branding` from the active policy with fallback to today's
  defaults. Wired into the REPL banner, version header, `/doctor`
  diagnostics title, GUI window title, and the system prompt
  (`{product}` placeholder substituted at load time so the model
  introduces itself as the org's product name). `{support_email}`
  template substitution available for any prompt that needs it.

- **Plugin/skill/MCP source allow-list** (Phase 2). New
  `policy/allowlist.rs` matcher with host+path glob patterns,
  segment wildcards, host-prefix wildcards (`*.acme.example`), and
  mid-segment globs (`skill-*`). Strips scheme / query / fragment /
  port / `.git` suffix before matching. Wired at:
  - `plugins::install` — rejects URLs not in `allowed_hosts`
  - `skills::install_from_url` — same gate, covers both git and zip
    dispatch paths
  - `skills::enforce_scripts_policy` — rejects skills with non-empty
    `scripts/` dirs when `allow_external_scripts: false`. Bundle path
    rejects scripted skills individually so declarative siblings still
    install.
  - `config::parse_mcp_json` — filters HTTP MCP servers whose URL host
    isn't in `allowed_hosts` when `allow_external_mcp: false`. Logs
    yellow `[mcp] '<name>' skipped: <reason>` to stderr. Stdio MCPs
    pass through (admin's mcp.json content = admin's responsibility).

- **Gateway enforcement** (Phase 3). When `policies.gateway.enabled:
  true`, every cloud-provider call routes through the org's private
  LLM gateway (LiteLLM, Portkey, Helicone, internal proxy). User's
  per-provider API keys are ignored — gateway owns credentials.
  Architecture: `build_provider` returns a single OpenAI-compatible
  client pointing at the gateway URL when active, regardless of which
  `ProviderKind` the user picked. Works because every common gateway
  product speaks OpenAI Chat Completions and routes to upstream
  providers via the `model` field.
  - Auth header template supports `{{env:NAME}}` for env-var-injected
    secrets (keeps gateway tokens out of the auditable signed policy
    file). `{{sso_token}}` placeholder reserved for Phase 4.
  - `read_only_local_models_allowed: true` escape valve lets local
    providers (Ollama, OllamaAnthropic, LMStudio, AgentSdk) bypass
    the gateway and run directly. Off by default (strict enterprise).
  - Validation gate at policy load: refuses to start if
    `gateway.enabled: true` with empty `url` (would otherwise
    fail-open at provider construction). Same check for
    `sso.enabled: true` with empty `issuer_url` / `client_id`.

- **`ENTERPRISE.md`** admin guide added to the public repo. Covers
  the open-core + signed-policy architecture, 10-minute quick-start
  walkthrough, operational concerns (key rotation, expiry, binary
  fingerprint binding, MDM deployment), troubleshooting all four
  startup-refusal modes, and an FAQ.

### Caveats

- **OIDC SSO is not yet implemented.** Phase 4 lands in v0.6.0. Until
  then, the gateway uses static-token / env-var auth via the
  `{{env:NAME}}` template substitution. Works fine for LiteLLM-style
  deployments where the gateway token is the only required credential.
- **Frontend branding strings** (5 hardcoded "thClaws" literals in
  `App.tsx`/`ChatView.tsx`, plus the embedded React-bundled logo
  imports) are NOT yet routed through the branding module. They land
  in a v0.5.x point release once the IPC `branding_get` bridge is
  wired. The Rust-side branding (REPL banner, GUI title, system
  prompt) is fully active in v0.5.0.
- **HTTP-layer fail-closed** for the gateway is currently advisory.
  The provider-replacement approach already eliminates bypass paths
  inside the agent loop. A wrapper `reqwest::Client` for
  defense-in-depth is a planned hardening pass.

## [0.4.2] — 2026-04-26

Small additive release in response to issue [#30](https://github.com/thClaws/thClaws/issues/30)
from Chawasit Tengtrairatana — same reporter who filed the
v0.4.1 Windows bug, with another high-quality writeup that
mapped cleanly to the existing catalogue layering.

### Added

- **User-defined context-window overrides.** A new `modelOverrides`
  block in `settings.json` (project + user, project wins per-key)
  lets the user pin context windows above every catalogue layer.
  Keyed by `provider/model` (e.g. `"anthropic/claude-sonnet-4-6"`).
  Useful for: (a) capping a local Ollama / LMStudio model to fit
  a smaller GPU than the model's native context, (b) per-provider
  variants of the same id (Anthropic vs OpenRouter for the same
  Claude model), (c) brand-new models not yet in the catalogue.
  Override resolution honors aliases in both directions and the
  same `vendor/` prefix-strip rules the catalogue uses.

- **`/models set-context` and `/models unset-context` slash
  commands.** Set: `/models set-context [--project] <provider/model>
  <size>` (size accepts `128000`, `128k`, or `1m`). Unset: `/models
  unset-context [--project] <provider/model>`. Default scope is
  user-global (`~/.config/thclaws/settings.json`); `--project`
  scopes to `.thclaws/settings.json`. Saves preserve every other
  field in the target file (atomic write).

- **`ContextSource` enum.** `effective_context_window_with` now
  returns `(u32, ContextSource)` distinguishing override hits from
  catalogue hits and from fallbacks. `/models` rendering marks
  override rows with a `source: "override"` stamp. Old `(u32,
  bool)` semantics remain available via `ContextSource::is_known()`.

### Policy: trust + warn

Overrides exceeding the catalogue value are accepted (the user
intent always wins) but a yellow warning is printed at save-time
so a typo doesn't silently produce upstream rejections at request
time. No clamp, no validation against the upstream-reported max —
matches the spirit of "user knows their hardware better than we do."

## [0.4.1] — 2026-04-27

Same-day patch release fixing a critical Windows-only bug surfaced
within hours of v0.4.0 shipping.

### Fixed

- **Bash tool unusable on Windows** ([#29](https://github.com/thClaws/thClaws/issues/29),
  Chawasit Tengtrairatana). `/bin/sh` was hardcoded at 4 sites
  (`tools/bash.rs`, `team.rs`, `repl.rs`, `hooks.rs`) — Windows
  doesn't have that path, so spawn returned `os error 3` (path
  not found) and the agent was effectively crippled on Win11.
  Centralized shell resolution into `util::shell_command_{sync,
  async}()`, branching on `cfg!(windows)`. On Windows this is
  `cmd.exe /C <cmd>`; on Unix `/bin/sh -c <cmd>`, unchanged.

### Added

- **`THCLAWS_SHELL` env override.** Power users with `bash` from
  WSL / Git Bash, or who prefer `pwsh`, can set
  `THCLAWS_SHELL="bash -c"` (or `"pwsh -Command"`, etc.). The
  helper splits on whitespace into `(executable, flag)`. Useful on
  Windows where `cmd.exe` doesn't parse bash-syntax commands the
  same as `bash` does.

### Caveats

Bash-syntax commands the agent emits (`find . -name '*.rs'`,
single-quoted args, complex pipelines) may not parse identically
under `cmd.exe`. Set `THCLAWS_SHELL="bash -c"` if you have Git Bash
or WSL `bash` on `PATH` for closer-to-Unix semantics on Windows.

## [0.4.0] — 2026-04-27

Minor release. Provider expansion + agent-loop UX polish + a class
of bugs around credential detection. Substantial accumulated work
from same-day batch PR processing across 7 community contributors.

### Added — Providers (4 new)

- **Z.ai (GLM Coding Plan).** OpenAI-compatible upstream at
  `https://api.z.ai/api/coding/paas/v4`. Routes via `zai/<id>`
  prefix, default `zai/glm-4.6`. API key in `ZAI_API_KEY`. Power
  users on the BigModel SKU can override via `ZAI_BASE_URL`.
  Closes [#14](https://github.com/thClaws/thClaws/issues/14).
- **LMStudio.** Local OpenAI-compatible runtime at `/v1`, default
  `http://localhost:1234/v1`. No auth. User-configurable base URL
  via Settings (mirrors the Ollama UX); env override
  `LMSTUDIO_BASE_URL`.
- **Azure AI Foundry** ([#21](https://github.com/thClaws/thClaws/pull/21),
  Parinya-chab / joparin). Anthropic-Claude-on-Azure via
  `{resource}/anthropic/v1/messages` with `x-api-key` auth. Reuses
  `AnthropicProvider` with a custom base URL — no duplicate stream
  code. Default model placeholder `azure/<deployment>` (Azure
  deployments are user-named); set via
  `/model azure/<your-deployment>` once `AZURE_AI_FOUNDRY_ENDPOINT`
  + `AZURE_AI_FOUNDRY_API_KEY` are configured. Forward-looking
  hooks added to `OpenAIProvider` (`with_api_key_header`,
  `with_list_models_url`) for a future Azure OpenAI provider.
- **Ollama Cloud** ([#28](https://github.com/thClaws/thClaws/pull/28),
  Av0cadoo). Hits `https://ollama.com/api/chat` with Bearer auth;
  reuses local Ollama's NDJSON parser. Round-trips the cloud-
  specific `thinking` field as a sibling on assistant messages
  (DeepSeek V4, Kimi K2.5, GLM-5, etc. emit reasoning content
  separately from the visible answer). 38 cloud-only models
  auto-discovered via the new catalogue-seed probe — including
  `deepseek-v4-flash`, `kimi-k2.5/2.6`, `glm-5/5.1`,
  `qwen3-coder-next`, `mistral-large-3:675b`, `gpt-oss:20b/120b`.
  Closes [#17](https://github.com/thClaws/thClaws/issues/17).

### Added — Agent-loop UX

- **AskUserQuestion GUI bridge** ([#16](https://github.com/thClaws/thClaws/pull/16),
  Kinzen-dev). The agent's `AskUser` tool used to fall through to
  invisible CLI stdin in the GUI — chat hung indefinitely. The
  question now appears as a chat-composer reply prompt; user
  reply routes back through a `oneshot` to the awaiting tool call.
  Falls back to CLI readline when no GUI is registered.
- **macOS Cmd+Q / Cmd+W shutdown shortcuts** ([#16](https://github.com/thClaws/thClaws/pull/16)).
  Two-layer coverage (frontend keydown listener + tao native
  KeyboardInput) so Cmd+Q reaches the SaveAndQuit save path even
  in fullscreen / focus-edge cases.
- **Post-key-entry model picker** ([#13](https://github.com/thClaws/thClaws/issues/13)).
  After successfully saving an API key in Settings, if the
  provider has a non-trivial catalogue (≥3 models, skipping
  runtime-loaded backends), a searchable modal opens so the user
  can pick a default model.
- **`/model` interactive picker on no-args** ([#25](https://github.com/thClaws/thClaws/issues/25),
  tkvision). Typing `/model` with no arguments now opens the
  same picker modal in addition to printing the current model.
  Reuses the post-key picker's UX. CLI-side TUI picker is a future
  follow-up.
- **Slash-command popup** ([#20](https://github.com/thClaws/thClaws/pull/20),
  siharat-th). Typing `/` in chat or terminal opens an
  autocomplete menu — built-in commands grouped by category
  (Session / Model / Context / Extensions / Team / System), plus
  user `.claude/commands/` and installed skills. Arrow keys
  navigate, Tab/Enter accept, Esc cancels. Smart Enter: only
  swallows Enter while composing the command name, falls through
  to submit once arguments are being typed.
- **Terminal caret-aware editing** ([#22](https://github.com/thClaws/thClaws/pull/22),
  siharat-th). Left/Right arrow keys, Home/End, Ctrl-A/Ctrl-E
  navigate the line buffer instead of echoing escape codes.
  Backspace and printable-char insertion are caret-aware: the
  fast `term.write(ch)` / `\b \b` path stays at end-of-line;
  mid-line edits redraw so the tail shifts correctly.

### Added — Catalogue

- **`agent/claude-opus-4-7-1m`** in the agent-sdk catalogue
  ([#26](https://github.com/thClaws/thClaws/issues/26), tkvision).
  Max-subscription users on the `agent/*` provider can now
  explicitly select the 1M-context Opus variant.
- **Ollama Cloud auto-discovery** in `catalogue-seed`. Probes
  `https://ollama.com/v1/models` when `OLLAMA_CLOUD_API_KEY` is
  set; refreshes 38 cloud rows every run.
- **`load_dotenv_walking_up()`** in `catalogue-seed` — walks up
  from cwd to find a workspace-root `.env`, so the operator tool
  picks up API keys regardless of which directory cargo is invoked
  from.

### Changed

- **Default Gemini model** `gemini-2.0-flash` → `gemini-2.5-flash`
  ([#27](https://github.com/thClaws/thClaws/pull/27), gokusenz).
  Google's deprecation page lists 2.0-flash as deprecated with
  shutdown 2026-06-01. Existing user configs that explicitly pin
  2.0-flash still work.
- **Read tool** now errors out clearly when bytes don't match any
  supported image format (PNG/JPEG/WebP/GIF) instead of guessing
  the MIME from the extension. Real images sniff fine; only the
  wrong-extension/corrupted unhappy path changes.

### Fixed

- **Empty `ANTHROPIC_API_KEY=""` (or any provider key) was treated
  as configured.** `std::env::var(...).is_ok()` returns true for
  an exported-but-empty value, so a stale shell rc / VS Code env
  injection blocked `auto_fallback_model` from switching when the
  user added a Gemini/Z.ai/etc. key. Both `kind_has_credentials`
  and `api_key_from_env` now require non-empty values; empty env
  falls through to the keychain. Includes a regression test
  `empty_env_var_treated_as_unset`.
- **`/exit` / `/quit` / `/q` slash commands** route through
  the backend `app_close` save path
  ([#16](https://github.com/thClaws/thClaws/pull/16)) instead of
  frontend-only `window.close()` after a 200 ms timeout.
- **Tool-bubble finalizer** searches backwards for the most recent
  unfinished tool bubble — handles text events arriving between
  `tool_use` and `tool_done`
  ([#16](https://github.com/thClaws/thClaws/pull/16)).
- **Frontend security hardening** from a same-day audit pass:
  10 MB cap on pasted/dropped images with inline error banner
  (was: silent drop, multi-MB paste froze the UI during base64
  encoding); 1 MB cap on terminal clipboard paste; explanatory
  threat-model comment on the `ReactMarkdown` call site;
  `ansiToHtml` documented invariant block.
- **Backend security hardening:** IPC `chat_user_message`
  attachment array bounded at `MAX_ATTACHMENTS_PER_MESSAGE = 10`
  + 67 MB total b64.

### Infrastructure

- **Branch protection ruleset on `main`** — block force-push +
  deletion (non-admin), require PR before merging, require status
  checks (cargo fmt + clippy + test (ubuntu-latest) + audit) to
  pass. Admin bypass for sync-from-private-workspace flow and
  emergency corrections.
- **Private Vulnerability Reporting (PVR)** enabled. SECURITY.md
  refreshed: PVR primary, email alternate, supported versions
  bumped 0.2.x → 0.3.x → 0.4.x.
- **CodeQL default setup** for JavaScript/TypeScript + Actions.
- **`cargo-audit` workflow** runs on PRs touching `Cargo.lock` +
  weekly cron.
- **Node 24 actions runtime opt-in** via
  `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` ahead of GitHub's
  2026-06-02 forced switch.
- **`ci.yml` permissions block** — `contents: read, actions: read`
  at top level (was inheriting GITHUB_TOKEN's default write
  scope; closes 4 CodeQL alerts).

### Acknowledged but deferred

- Copy-button-on-chat-bubble surface scope decision (toast / scope
  restriction / pattern-redaction) — captured in the audit reports
  under `dev-log/103-security-audit-frontend.md`.
- IPC message types still stringly-typed; discriminated-union
  refactor queued.
- Transitive `glib` 0.18.5 / gtk-rs 0.18.x unmaintained warnings
  remain pending the upstream `wry`/`webkit2gtk` GTK4 migration.
- CLI TUI picker for `/model` no-args
  ([#25](https://github.com/thClaws/thClaws/issues/25)) — GUI
  side ships in this release; CLI is future work.
- GitHub Copilot provider
  ([#24](https://github.com/thClaws/thClaws/issues/24)) — needs
  GitHub OAuth web flow; queued for a future minor.
- `output.log` should record tool-call argument detail
  ([#23](https://github.com/thClaws/thClaws/issues/23)).

## [0.3.5] — 2026-04-26

Same-day feature/fix follow-up to v0.3.4: two new providers, the
post-key-entry model picker, plus a real bug fix for users whose
shell rc / VS Code env injects a blank `ANTHROPIC_API_KEY`.

### Added

- **Z.ai (GLM Coding Plan) provider.** OpenAI-compatible upstream
  at `https://api.z.ai/api/coding/paas/v4`. Models route via
  `zai/<id>` prefix (default `zai/glm-4.6`). API key in
  `ZAI_API_KEY`. Power users on the BigModel SKU can override the
  endpoint via `ZAI_BASE_URL`. Closes [#14](https://github.com/thClaws/thClaws/issues/14).
- **LMStudio provider.** Local-runtime, OpenAI-compatible at `/v1`.
  No auth. User-configurable base URL via Settings (default
  `http://localhost:1234/v1`); env override `LMSTUDIO_BASE_URL`.
  Mirrors the Ollama UX so changing port doesn't require a
  settings.json edit.
- **Post-key-entry model picker** ([#13](https://github.com/thClaws/thClaws/issues/13)).
  After successfully saving an API key in Settings, if the
  provider has a non-trivial catalogue (≥3 models, skipping
  runtime-loaded backends like Ollama/LMStudio), a searchable
  modal opens so the user can pick a default model directly —
  instead of landing on whatever `auto_fallback_model` chose.
  Skip / Esc / click-outside leaves the auto-pick in place.
- **AskUserQuestion GUI bridge** ([#16](https://github.com/thClaws/thClaws/pull/16),
  Kinzen-dev). The agent's `AskUser` tool used to fall through to
  invisible CLI stdin in the GUI — chat hung indefinitely. The
  question now appears as a chat-composer reply prompt; user
  reply routes back through a `oneshot` to the awaiting tool call.
  Falls back to CLI readline when no GUI is registered.
- **macOS Cmd+Q / Cmd+W shutdown shortcuts** ([#16](https://github.com/thClaws/thClaws/pull/16)).
  Two-layer coverage (frontend keydown listener + tao native
  KeyboardInput) so Cmd+Q reaches the SaveAndQuit save path even
  in fullscreen / focus-edge cases.

### Fixed

- **Empty `ANTHROPIC_API_KEY=""` (or any provider key) was treated
  as configured.** `std::env::var(...).is_ok()` returns true for an
  exported-but-empty value, so a stale shell rc / VS Code env
  injection blocked `auto_fallback_model` from switching when the
  user added a Gemini/Z.ai/etc. key. Both `kind_has_credentials`
  and `api_key_from_env` now require non-empty values; empty env
  falls through to the keychain. Includes a regression test
  (`empty_env_var_treated_as_unset`).
- **`catalogue-seed` reads workspace-root `.env`.** When invoked
  via `cargo run --bin catalogue-seed` from a nested crate dir,
  the binary now walks up from cwd to find the workspace's `.env`
  and load API keys from it. Added
  `dotenv::load_dotenv_walking_up()`.
- **Tool-bubble finalizer searches backwards for unfinished tools**
  ([#16](https://github.com/thClaws/thClaws/pull/16)). Old code
  assumed `messages[last]` was the matching tool bubble; failed
  when text or other events arrived between `tool_use` and
  `tool_done`.
- **`/exit` / `/quit` / `/q` slash commands** now route through
  the backend `app_close` save path ([#16](https://github.com/thClaws/thClaws/pull/16))
  instead of frontend-only `window.close()` after a 200 ms timeout.

### Internal

- New `model_set` IPC handler — frontend-driven model change path,
  used by the new picker; mirrors what `/model` does in the agent
  loop. Available for any future picker UI.
- Dotenv `load_dotenv_walking_up(start)` helper exposed for
  operator-tool scenarios.

## [0.3.4] — 2026-04-26

Same-day hardening patch following an internal security audit of v0.3.3.
No new features; all changes are defensive limits and clearer errors on
the image-attachment and terminal-paste paths.

### Added

- **Inline error feedback on image attachment.** Pasting or dropping an
  unsupported image type or an image larger than 10 MB now shows a
  short auto-clearing banner ("Image too large: 17.3 MB (max 10 MB)")
  instead of silently dropping. Same path covers
  `image/svg+xml`/etc. → "Unsupported image type".

### Changed

- **Read tool errors cleanly on wrong-extension image files.** Files
  like `screenshot.png` containing non-PNG bytes used to slip through
  with a guessed MIME and get rejected by the provider with an opaque
  400. They now fail at Read with a pointed error message
  ("bytes don't match any supported image format despite extension
  claiming image/png — file may be corrupted, encrypted, or saved
  with the wrong extension"). Real images with these extensions are
  unaffected.

### Fixed (security hardening)

- **ChatView image paste/drop:** 10 MB per-attachment cap. Above the
  cap, the image is rejected with a visible error rather than ballooning
  the IPC payload and freezing the UI during base64 encoding.
- **TerminalView clipboard paste:** 1 MB cap. Multi-MB pastes used to
  freeze the main thread during synchronous `atob()` + `TextDecoder`;
  oversized pastes are now dropped with a console warning.
- **Backend IPC `chat_user_message` attachment array:**
  `MAX_ATTACHMENTS_PER_MESSAGE = 10` and a 67 MB combined-base64 cap.
  Defense-in-depth against a malicious or buggy frontend bypassing
  the per-image cap; worst-case payload now bounded at ~50 MB raw
  per message rather than unbounded.
- **`TeamView.tsx` `ansiToHtml`:** documented the escape-first
  invariant in a JSDoc block. The function's output is consumed via
  `dangerouslySetInnerHTML`; preserving HTML-escape-before-tag-build
  ordering is what keeps it safe. Block lists three changes to NOT
  make.
- **Markdown rendering threat-model comment** added at the
  `ReactMarkdown` call site documenting that `msg.content` is
  untrusted model output and the configured plugin chain
  (`remark-gfm`, `rehype-highlight`) is intentionally the safe stack
  — no `allowDangerousHtml`, no `rehype-raw`.

### CI / Infrastructure

- **Workflow least-privilege.** `ci.yml` now declares an explicit
  top-level `permissions: contents: read, actions: read`, instead of
  inheriting the GITHUB_TOKEN's default write scope. Closes 4
  CodeQL alerts (`actions/missing-workflow-permissions`).
- **CodeQL Rust scan** actually runs now: added `libdbus-1-dev` +
  `pkg-config` install before `cargo build`. The keychain crate's
  transitive `libdbus-sys` was failing pkg_config detection, breaking
  every prior CodeQL Rust run before extraction even started.
- **Node 24 actions runtime opt-in** via
  `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` on both `release.yml`
  and `ci.yml`. Surfaces any action-runtime breakage on our schedule
  rather than at GitHub's 2026-06-02 forced cutover.

### Known issues — acknowledged but deferred

- Copy-button surface on chat bubbles (system/tool/assistant) doesn't
  warn or filter when copying messages that may contain previously-
  pasted secrets. Needs a design choice (toast confirmation vs.
  scope restriction vs. pattern-based redaction); deferred to v0.3.5.
- IPC message types are still stringly-typed; discriminated-union
  refactor queued for a future maintenance pass.
- Transitive `glib` 0.18.5 / gtk-rs 0.18.x unmaintained warnings
  (12 RustSec entries) remain pending the upstream `wry`/`webkit2gtk`
  GTK4 migration.

## [0.3.3] — 2026-04-26

Feature release rolling up image attachment across providers, chat UI
polish, and a community-PR sweep that ran `pnpm lint` to clean. Plus
a transitive postcss XSS patch and a docs-prerequisite correction.

### Added

- **Image attachment across providers.** The Read tool now returns
  inline images for vision-capable models (PNG/JPG/GIF/WebP). Wire
  shaping is per-provider:
  - **Anthropic** — native via serde, zero provider code.
  - **OpenAI** — synthetic user message with `image_url` blocks
    referencing the originating `tool_call_id` (their tool-role
    messages can't carry images).
  - **Gemini** — `inlineData` parts as siblings to `functionResponse`
    in the same content.
  - **Ollama / OpenAI Responses** — text-only flatten on wire (no
    pixels to model).
- **ChatView attachments.** Paste and drag-drop image files into the
  chat composer; thumbnails preview before send.

### Changed

- **Chat rendering.** Assistant turns render as markdown
  (headings/lists/code/tables) instead of raw text. Tool output
  collapses to compact one-line indicators by default, with errors
  always shown in full.
- **Tool result handling on history restore.** `tool_result` blocks
  are dropped on session reload; `tool_use` rendering is unified
  across the streaming and reload paths.

### Fixed

- **postcss 8.5.9 → 8.5.10** ([GHSA-qx2v-qp2m-jg93](https://github.com/advisories/GHSA-qx2v-qp2m-jg93)).
  Transitive frontend dep; thClaws ships pre-compiled Tailwind so
  runtime exposure was minimal but Dependabot was flagging.
- **Documented Rust prerequisite: 1.78 → 1.85** in user-manual.
  The `home` crate v0.5.12 (transitive) needs edition 2024, so the
  effective MSRV moved to 1.85. README + CONTRIBUTING were already
  updated in [#3](https://github.com/thClaws/thClaws/pull/3); this
  catches the user-manual files that PR missed.
- **Read tool: image format sniffing from magic bytes** instead of
  trusting file extensions (which lie often enough — `.jpg` files
  that are actually PNGs, etc.).
- **OpenAI batched tool messages.** Emit batched tool messages
  back-to-back with a single combined image follow-up, instead of
  interleaving.
- **Sidebar.tsx unreachable branch.** Duplicate `sessions_list`
  `else if` removed (#4).
- **Frontend lint sweep** by [@parintorns](https://github.com/parintorns)
  in #4, #6, #7, #8, #9, #10 — `react-hooks/exhaustive-deps`,
  `react-refresh/only-export-components`, `no-empty`, type safety
  in IPC bridge and TeamView. `pnpm lint` is now clean.
- **`.gitignore`: `.thclaws/sessions/` → `.thclaws/`.** Was leaking
  `team/`, `settings.json`, and similar runtime files into
  `git status` (#6).

### Infrastructure

- **Workspace `Cargo.toml` at repo root** by
  [@bombman](https://github.com/bombman) (#2). `cargo build` now
  works from the repo root as the README documents; build output
  is at `target/release/` instead of `crates/core/target/release/`.

## [0.3.2] — 2026-04-25

Patch release fixing two GUI startup-recovery bugs surfaced in the
hours after v0.3.1 shipped. Both reach the user before they've typed
their first prompt, so this release is recommended for everyone on
v0.3.1 — particularly Linux users, who can't launch v0.3.1 at all.

### Fixed

- **Linux GUI startup panic.** v0.3.1 panicked at startup on every
  Linux build with `webview build: UnsupportedWindowHandle`
  (reported on Ubuntu 22.04). `wry` can't construct a WebKit2GTK
  webview from a raw window handle the way it does on macOS / Windows
  — WebKit2GTK is a GTK widget that has to be packed into a GTK
  container. Fixed by switching to `wry`'s Linux-only
  `build_gtk(window.default_vbox().unwrap())` behind
  `#[cfg(target_os = "linux")]`. The cross-platform path is preserved
  for macOS / Windows. (commits 6171815 by @Phruetthiphong + 729538b)
- **First-time API key setup required an app restart.** Pasting a
  provider key in Settings on a fresh install would update the sidebar
  to show the new provider, but the running agent kept holding the
  stale (or no-op) provider it was constructed with at startup —
  resulting in "sidebar shows openai but error mentions anthropic"
  on the first send. Two fixes:
  - The shared-session worker no longer exits on missing-key startup;
    it installs a `NoopProvider` placeholder and stays alive so a
    later config reload can swap in a real provider.
  - Added `ShellInput::ReloadConfig`. The `api_key_set` and
    `api_key_clear` IPC handlers now send it after their save, so the
    worker reloads `AppConfig`, rebuilds the agent's provider in
    place, and broadcasts the sidebar update — all without an app
    restart. (commit 27d163d)

## [0.3.1] — 2026-04-25

Re-release of v0.3.0 — the v0.3.0 tag's release workflow failed
(missing `banner.txt` broke the frontend build). Tag re-cut against
the fix.

### Fixed (v0.3.1 vs v0.3.0)

- **`banner.txt` now ships in the repo** so `vite build` resolves
  `import bannerText from "../../../banner.txt?raw"` in
  `TerminalView.tsx`. v0.3.0 release job failed at this step on every
  platform.
- **`cargo fmt` drift** in `crates/core` cleaned up so the CI fmt
  check passes.
- **`actions/checkout`, `actions/setup-node`, `actions/upload-artifact`,
  `actions/download-artifact` bumped to v5** for Node 24 support
  (v4 is now deprecated on GitHub-hosted runners).

### Providers (since v0.2.2)

- Reasoning-model support end-to-end: DeepSeek v4-flash/pro, DeepSeek r1,
  OpenAI o-series via OpenRouter. `reasoning_content` is captured into a
  Thinking content block and echoed back on subsequent turns (these
  providers 400 without it). Conservative allowlist — non-thinking models
  pay zero extra tokens.
- Provider-aware alias resolution: agent-def `model: sonnet` stays in
  the project's current provider namespace instead of surprise-switching
  to native Anthropic.
- Model catalogue v3 (provider-keyed maps, real ids, per-row provenance).
  `/models` reads from catalogue; `/model` auto-scans Ollama context
  window.

### Agent Teams (since v0.2.2)

- Sandbox boundary anchors to `$THCLAWS_PROJECT_ROOT` (not cwd); worktree
  teammates can write shared artifacts at workspace root; `Write` into
  deep new trees walks up to the longest existing ancestor.
- "Project settings win" on cwd change: GUI reloads `ProjectConfig` and
  rebuilds the agent; worktree teammates pick up the workspace's
  `settings.json` (was silently falling back to user config).
- Role guards on `Bash` / `Write` / `Edit`:
  - Lead can't run `rm -rf`, `git reset --hard`, `git worktree remove`,
    `git push --force`, `git checkout -- …`, or `Write` / `Edit` source
    files. One narrow exception: when a merge is in progress and the
    target file has `<<<<<<<` markers, lead may write the resolved
    content (so package.json-style conflicts can be handled without
    delegating).
  - Teammates can't `git reset --hard <other-branch>`. Same-branch
    recovery (`HEAD~N`, sha, tags) stays allowed.
- `EDITOR` / `GIT_EDITOR` / `VISUAL` / `GIT_SEQUENCE_EDITOR` stubbed to
  `true` for teammates so `vi` / `git commit -e` don't hang waiting for
  input via `/dev/tty`.
- "Plan Approval" convention documented in default `lead.md` /
  `agent_team.md` prompts (lead↔teammate handshake, NOT a user gate).
- `TeamTaskCreate` gains an `owner` field; `claim_next` is role-aware.

### GUI (since v0.2.2)

- Terminal tab: Up/Down arrow prompt history.
- Files tab: WYSIWYG round-trip for `.md` preview + editor; HTML preview
  base-URL fix; off-screen edit-button positioning fix.
- Approval modal; MCP spawn through approval sink; `ReadyGate` for
  deferred startup so the worker accepts prompts before MCP-spawn
  approval returns.
- Context warning banner + per-file size breakdown of the system prompt.
- Settings menu polish: accent-tinted hover + focus highlight; modal
  backdrop dismiss on mousedown-origin (fewer accidental closes).
- Windows GUI fixes backported from upstream: `rfd` file picker,
  `native_dialog` confirm, `ospath()` path-separator helper.

### KMS

- `/kms ingest` slash command; sidebar refreshes live on KMS changes.

### Catalogue tooling

- New `make catalogue` target wraps `catalogue-seed` with a diff-stat
  preview and a per-provider transparency report (new IDs added +
  unchanged + skipped-no-context counts).

### User manual — NEW in this release

- 17-chapter reference manual in English (`user-manual/`) and Thai
  (`user-manual-th/`) with shared images at `user-manual-img/`. Covers
  installation through agent teams. Case-study chapters (18–24) for
  building/deploying real projects remain in workspace draft and will
  graduate to the published manual as each is reviewed.

## [0.2.2] — 2026-04-22

### Added

- **Shared in-process session backing both GUI tabs.** Terminal and Chat tabs now share one Agent + Session + history; typing in either contributes to the same conversation, and `/load` replays the transcript into both.
- **Every REPL slash command works from the GUI.** `/model`, `/provider`, `/permissions`, `/thinking`, `/compact`, `/doctor`, `/mcp`, `/plugin`, `/skill`, `/kms`, `/team`, and the rest all execute identically in Terminal, Chat, and CLI.
- **Live activation for mutations** (no restart required): `/mcp add` spawns the subprocess and registers its tools; `/skill install` refreshes the store and updates the system prompt; `/plugin install` picks up plugin-contributed skills immediately; `/kms use` / `/kms off` register and deregister tools on the fly.
- **Agent Teams toggle in the Settings menu** — one-click on/off for `teamEnabled` without editing `settings.json`.
- **Light/dark/system theme** — click the gear icon → Appearance. Covers app chrome, xterm terminal palette, CodeMirror editor, and Markdown preview; persists to `~/.config/thclaws/theme.json`.
- **Files-tab viewer + editor** — syntax-highlighted preview (CodeMirror 6, ~40 languages), GFM markdown preview (comrak), TipTap markdown editor, CodeMirror code editor with dirty-state tracking and Cmd/Ctrl+S save.
- **Chat tab welcome logo.** Team tab is always visible with an empty-state pointer.

### Fixed

- **Windows startup hang at the secrets-backend dialog.** Every `std::env::var("HOME")` site now goes through a cross-platform `home_dir()` helper that understands `%USERPROFILE%` and `%HOMEDRIVE%%HOMEPATH%`. Previously the silent `Error::Config("HOME is not set")` left the user staring at a silently re-enabled button.
- **Multi-line paste in Terminal tab** submits as one prompt instead of firing one `shell_input` per line.
- **Terminal assistant output concatenates** during streaming — previously each chunk erased the previous one.
- **ANSI escape codes stripped from Chat bubbles** — slash-command output (`render_help`) no longer shows `[2m...[0m` junk.
- **Ctrl+C on empty line cancels the in-flight turn** (was a no-op after the shared-session refactor).
- **Team tab auto-shows** after `TeamCreate` — no longer gated on `teamEnabled`.
- **`/provider X` falls back to the first available model** if the hardcoded default isn't in the live catalogue. `/model X` stays strict so typos fail loud.
- **System-prompt grounding on `agent/*` provider** — the SDK subprocess doesn't receive thClaws's tool registry; when the user asks for teams from `agent/*`, the model is told honestly that team tools are unreachable and to switch provider.

### Removed

- **`managed/*` (Anthropic Managed Agents cloud) provider.** The Managed Agents API is designed for deploying long-running agents to Anthropic's cloud with server-side tool execution — a poor fit for a local interactive CLI where tool calls should hit the user's filesystem.

### Diagnostics

- `THCLAWS_DEVTOOLS=1` opens the WebView devtools so users can Inspect → Console on a blank screen.
- Startup modal shows a diagnostic card after 3 seconds of IPC dead-air, listing `window.ipc` availability, platform, and UserAgent — instead of an indefinite blank screen.

## [0.2.1] — 2026-04-21

First public open-source release — version and date will be set on tag.

### Agent core

- **Native Rust agent loop** — single-binary distribution for macOS, Windows, Linux
- **Streaming provider abstraction** — token-by-token output to the UI, tool-use assembly across chunks
- **History compaction** — automatic when context approaches the configured budget, preserves semantic coherence
- **Permission modes** — `auto`, `ask`, `accept-all` with per-tool approval flow
- **Hooks** — shell commands triggered on agent lifecycle events (before-tool, after-response, etc.)
- **Retry loop with exponential backoff** — skips retries on config errors to surface actionable messages immediately
- **Max-iteration cap** — prevents runaway tool-call loops
- **Compatible session format** (JSONL, append-only) with rename and load-by-name

### Providers

- **Anthropic Claude** — with extended thinking (budget-configurable), prompt caching, and Claude Code CLI bridge
- **OpenAI** — Chat Completions and Responses API
- **Google Gemini** — including multi-byte-safe streaming
- **DashScope / Qwen**
- **Ollama** (local, also exposed as Ollama-Anthropic for drop-in compatibility)
- **Agentic Press LLM gateway** — first-class provider with fixed URL
- **Multi-provider switching mid-session** via `/provider` and `/model`
- **Model validation** — `/model NAME` verifies availability against the active provider before committing
- **Auto-fallback at startup** — picks the first provider with credentials if the configured model has no key

### Tools

- File: `Read`, `Write`, `Edit`, `Glob`, `Ls`, `Grep`
- Shell: `Bash` (with timeout, sandboxed cwd)
- Web: `WebFetch`, `WebSearch` (Tavily / Brave / DuckDuckGo / auto)
- User interaction: `AskUserQuestion`, `TodoWrite`
- Planning: `EnterPlanMode`, `ExitPlanMode`
- Delegation: `Task` (subagent with recursion up to `max_depth`)
- Knowledge: `KmsRead`, `KmsSearch`
- Team coordination: `SpawnTeammate`, `SendMessage`, `CheckInbox`, `TeamStatus`, `TeamCreate`, `TeamTaskCreate`, `TeamTaskList`, `TeamTaskClaim`, `TeamTaskComplete`
- Tool filtering via `allowedTools` / `disallowedTools` in config

### Claude Code compatibility

- Reads `CLAUDE.md` and `AGENTS.md` (walked up from `cwd`)
- `.claude/skills/`, `.claude/agents/`, `.claude/rules/`, `.claude/commands/`
- `.thclaws/` counterparts: `.thclaws/skills/`, `.thclaws/agents/`, `.thclaws/rules/`, `.thclaws/AGENTS.md`, `.thclaws/CLAUDE.md`
- `.mcp.json` at project root (primary) and `.thclaws/mcp.json`
- `~/.claude/settings.json` fallback for users migrating from Claude Code
- Permission shapes: string (`"auto"` / `"ask"`) and Claude Code object (`{allow, deny}` with `Tool(*)` globs)

### Built-in KMS (Knowledge Management System)

- Karpathy-style personal / project wikis under `~/.config/thclaws/kms/` and `.thclaws/kms/`
- Multi-select active list in `.thclaws/settings.json` — multiple KMS feed a single chat
- `index.md` injected into the system prompt; pages pulled on demand via `KmsRead` / `KmsSearch`
- No embeddings in v1 (grep + read); hosted embeddings planned for future RAG upgrade
- Slash commands: `/kms`, `/kms new [--project] NAME`, `/kms use`, `/kms off`, `/kms show`
- Sidebar checkbox UI for attach / detach

### Agent Teams

- Multi-agent coordination via tmux session with a GUI layer
- Role separation: `lead` coordinator + `teammate` executors
- Mailbox-based message passing
- Team tasks (create / list / claim / complete)
- Opt-in via `teamEnabled: true` in settings
- Worktree isolation — teammates can run in separate git worktrees

### Plugin system

- Install from git URL or `.zip` archive
- Enable / disable / show
- Plugins contribute skills, commands, agents, and MCP servers under one manifest
- Project-scope and user-scope installations
- `/plugin` slash command family (install / remove / enable / disable / show)

### MCP (Model Context Protocol)

- stdio transport (spawned subprocess)
- HTTP Streamable transport
- OAuth 2.1 + PKCE for protected MCP servers
- `/mcp add [--user] NAME URL`, `/mcp remove [--user] NAME`
- Discovered tools namespaced by server name

### Skills

- Claude Code's skill format (`SKILL.md` with frontmatter)
- Project, user, and plugin scopes (all merged)
- Exposed as a `Skill` tool AND as slash-command shortcuts (`/skill-name`)
- `/skill install [--user] <git-url-or-.zip> [name]` for installing remote skills
- Skill catalog surfaced in the system prompt

### Desktop GUI

- Native `wry` webview + `tao` windowing (not Electron)
- React + Vite frontend built as a single HTML file
- Sidebar: provider status, active model, sessions, MCP servers, knowledge bases
- Chat panel with streaming text rendering
- xterm.js terminal tab with native clipboard bridge (`arboard`) — Cmd/Ctrl+C/X/V/A/Z
- Ctrl+C heuristic: clears current line when non-empty, otherwise passes SIGINT
- Files tab
- Team view tab (tmux pane preview)
- Settings menu (gear popup): Global instructions, Folder instructions, Provider API keys
- Tiptap-based Markdown editor for AGENTS.md (round-trip through `tiptap-markdown`)
- Startup folder modal — pick working directory on launch
- Provider-ready indicator (green / red dot + strike-through when no key)
- Auto-switch model to a working provider when a key is saved
- Session rename with inline pencil button; `/load by name`
- Turn duration display after each assistant response

### Memory

- Persistent memory store at `~/.config/thclaws/memory/`
- Four memory types: user, feedback, project, reference
- `MEMORY.md` index auto-maintained
- `/memory list`, `/memory read NAME`
- Frontmatter-based classification so future conversations recall relevance

### Secrets & security

- OS keychain integration (macOS Keychain / Windows Credential Manager / Linux Secret Service)
- **Secrets-backend chooser** — first launch asks OS keychain or `.env`
- Single-entry keychain bundle — all provider keys in one item, one ACL prompt per launch
- `.env` fallback when keychain is unavailable (e.g. headless Linux)
- Cross-process key visibility — GUI and PTY-child REPL read the same keychain entry
- Precedence: shell export > keychain > `.env` file
- Sandboxed file tool operations (path-traversal rejection)
- Permission system protects destructive operations
- Env toggles: `THCLAWS_DISABLE_KEYCHAIN` (test opt-out), `THCLAWS_KEYCHAIN_TRACE` (diagnostics)

### Observability

- Per-provider, per-model token usage tracking (`/usage`)
- Turn duration surfaced after each LLM response
- Optional raw-response dump to stderr (`THCLAWS_SHOW_RAW=1`)
- Keychain trace logs for cross-process debugging

### Developer experience

- Slash commands: `/help`, `/clear`, `/history`, `/model`, `/models`, `/provider`, `/providers`, `/config`, `/save`, `/load`, `/sessions`, `/rename`, `/memory`, `/mcp`, `/plugin`, `/plugins`, `/tasks`, `/context`, `/version`, `/cwd`, `/thinking`, `/compact`, `/doctor`, `/skills`, `/skill`, `/permissions`, `/team`, `/usage`, `/kms`
- Shell escape: `! <command>` runs a shell command inline
- `--print` / `-p` non-interactive mode for scripting
- `--resume SESSION_ID` (or `last`) to pick up where you left off
- `--team-agent NAME` for spawning teammates
- Graceful startup — REPL opens with a friendly placeholder if no API key is configured
- Dual CLI + GUI from the same binary
- Compile-time default prompts with `.thclaws/prompt/` overrides

---

*Development prior to 0.2.0 was internal. The public history starts with this release.*
