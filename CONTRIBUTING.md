# Contributing to thClaws

Thanks for your interest — we welcome contributions from everyone. Please read this quick guide before opening an issue or pull request.

## Ways to contribute

- **Bug reports** — file an issue with the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md). Minimal reproduction steps make a big difference.
- **Feature requests** — file an issue with the [feature request template](.github/ISSUE_TEMPLATE/feature_request.md). Explain the problem, not just the proposed solution.
- **Documentation** — typo fixes, clarity improvements, and examples are always welcome.
- **Code** — bug fixes, performance improvements, and feature implementations. For anything non-trivial please open an issue first to align on approach.
- **Plugins / Skills / MCP servers** — extend thClaws via its plugin system without modifying the core.

## Development setup

**Prerequisites:** Rust 1.85+, Node.js 20+, pnpm 9+.

```sh
git clone https://github.com/thClaws/thClaws.git
cd thClaws

# Build frontend
cd frontend && pnpm install && pnpm build && cd ..

# Build + test Rust
cargo build --features gui
cargo test --features gui
```

Useful commands:

| Command | Purpose |
|---|---|
| `cargo test --features gui` | Run the full test suite |
| `cargo fmt --check` | Verify formatting |
| `cargo clippy --features gui -- -D warnings` | Lint |
| `cd frontend && pnpm tsc --noEmit` | Type-check frontend |
| `cargo run --features gui` | Launch the GUI |
| `cargo run -- --cli` | Launch the CLI |

## Pull request workflow

1. **Fork** the repo and create a feature branch from `main`.
2. **Commit** with a descriptive message. We loosely follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`) but it's not strictly enforced.
3. **Test** — make sure `cargo test --features gui` passes and there are no new warnings.
4. **Format + lint** — `cargo fmt` and address clippy findings.
5. **Push** and open a PR using the [PR template](.github/PULL_REQUEST_TEMPLATE.md).
6. **Link issues** — reference any issues the PR closes or relates to.

## Code style

- Default to **no comments**. Only add a comment when the *why* is non-obvious.
- Favor **small, focused PRs** over large sweeping changes.
- Match existing style in the surrounding code.
- Keep changes **in-scope** — don't refactor unrelated code in the same PR.
- For new features, **add tests** alongside the implementation.

## Commit attribution

We accept contributions under the dual MIT / Apache-2.0 license (see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)). By submitting a pull request, you agree that your contribution is licensed under the same terms.

## Community

- **GitHub Discussions** — open-ended questions and ideas
- **GitHub Issues** — bug reports, feature requests
- **Email** — security and sensitive topics: security@thaigpt.com

## Code of Conduct

By participating in this project you agree to abide by the [Code of Conduct](CODE_OF_CONDUCT.md).
