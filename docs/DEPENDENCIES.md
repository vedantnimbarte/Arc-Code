# Dependency Notes & Tracked Debt

Known dependency issues, why they exist, and the conditions under which they
resolve.

The **`audit` job is required** — a new RUSTSEC advisory fails the build. What
we knowingly tolerate is listed in `.cargo/audit.toml`, each entry naming the
crate, the reason, and the way out (mirrored below).

It runs cargo-audit rather than `cargo deny check advisories`: cargo-deny does
not flag the `unsound` advisories currently open against `lru`, so swapping it
in would quietly reduce coverage. `deny.toml` therefore owns licences, bans,
and sources only, and the **`deny` job** stays informational until the reqwest
duplicate clears, at which point `[bans].multiple-versions` flips to `"deny"`.

## `reqwest` — migrated to 0.13 (first-party unified)

**Status:** done for everything we control. All five first-party crates
(`wingman-cli`, `wingman-mcp`, `wingman-providers`, `wingman-tools`,
`wingman-tui`) are now on **reqwest 0.13**, sharing one copy with `rmcp` (the
MCP client, which was already on 0.13). The `rustls-tls` feature was renamed to
`rustls` in 0.13, and that feature now pulls `aws-lc-rs` (needs `cc`/NASM) — so
we instead use `rustls-no-provider` and install the **ring** crypto provider
once at startup (`wingman_core::ensure_tls_provider`), preserving the
static-binary / no-OpenSSL distribution story. `.form()` moved behind reqwest's
`form` feature (enabled on `wingman-providers` for the watsonx IAM exchange).

**Remaining 0.12 copy — outside our control:** `hf-hub` (pulled by `fastembed`
behind the optional `embeddings` feature of `wingman-rag`, for downloading
embedding models) still depends on `reqwest 0.12`. So a second reqwest major
persists *only* in the embeddings dependency tree. It clears when `hf-hub` bumps
to 0.13, or entirely if you build `wingman-rag` with `--no-default-features`
(hash embedder, no fastembed). `deny.toml` keeps `multiple-versions = "warn"`
until then.

## `ort` release-candidate (`2.0.0-rc.x`)

**Status:** tracked; not directly in our control.

`ort` (ONNX Runtime bindings) is pulled by `fastembed` for local embeddings
behind the `embeddings` feature of `wingman-rag`. It is a release candidate in a
shipping product, and its prebuilt binary dictates the **glibc ≥ 2.38** floor
documented in the CI/release comments.

**Resolution:** upgrade when `fastembed` ships a stable `ort`. We don't depend on
`ort` directly, so we can't pin ahead of `fastembed`. The hash-embedder fallback
(`--no-default-features` on `wingman-rag`) avoids `ort` entirely for builds that
don't need semantic search.

## GPL-3.0 in the optional `chrome` feature

**Status:** contained by build configuration; do not redistribute a browser
build.

`headless_chrome` (optional, behind `wingman-browser`'s `chrome` feature, which
the CLI exposes as `--features browser`) pulls **`auto_generate_cdp`, licensed
GPL-3.0-or-later**. Wingman ships under Apache-2.0, and those are not
compatible for redistribution: a binary built with `--features browser` links
copyleft code and cannot be distributed under Apache-2.0.

It does not affect anyone using a release binary — the feature is off by
default and no published artifact enables it.

`deny.toml` sets `[graph] all-features = false` so the licence check reflects
what actually ships. The deliberate trade-off: the `chrome` feature's own
dependency tree is **not** licence-checked in CI. Treat browser verification as
a local-only tool.

**Resolution:** either drop `headless_chrome` for a CDP client with a
permissive licence, or keep the feature permanently local-only and never
publish a binary built with it.

## Advisories we currently tolerate

Each of these is an `[advisories].ignore` entry in `.cargo/audit.toml`.

| Advisory | Crate | Why it stands | Clears when |
| --- | --- | --- | --- |
| RUSTSEC-2025-0009 | `ring` 0.17.9 | AES functions may panic under overflow checking. Fixed in ring ≥ 0.17.12, which needs `cc ^1.2.8`; we are held at `cc` 1.0.x by `tree-sitter-javascript` 0.21.4 (`cc = "~1.0.90"`). | The tree-sitter 0.22 → 0.23 migration lands. This is the real blocker, not ring. |
| RUSTSEC-2024-0436 | `paste` 1.0.15 | Unmaintained proc-macro, pulled transitively by `tokenizers` → `fastembed`. No first-party code touches it. | `fastembed`/`tokenizers` drop it upstream. |
| RUSTSEC-2026-0002, RUSTSEC-2026-0253 | `lru` 0.12.5 | Two unsoundness advisories (`IterMut` stacked-borrows; panic safety in `pop()`). Pinned by `ratatui` 0.29.0 for its internal line-wrap cache; we never call `lru` directly. | `ratatui` bumps its `lru` dependency. |
| RUSTSEC-2026-0192 | `ttf-parser` 0.25.1 | Unmaintained font parser, reached only through `pdf-extract` (glyph-to-character mapping during text extraction). No first-party code touches it. | `pdf-extract` moves to a maintained parser, or the `pdf` feature is dropped. |

## Policy

New advisories or new duplicate majors should be triaged here with a crate name,
a reason, and a resolution condition — not silently ignored. Add advisory
ignores to `.cargo/audit.toml` only with a comment naming the crate, the
reason, and what has to change for the entry to go away. An ignore without a
way out is a permanent hole, not a triage decision.
