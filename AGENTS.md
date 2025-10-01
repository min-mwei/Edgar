# Edgar Codebase Notes

- This repository is a fork of the upstream Codex CLI.
- Edgar is a standalone program and should **not** reuse the Codex home directory.
- Configuration, history, and auth state live under `~/.edgar/` (or the path pointed to by `CODEX_HOME`).
- When following upstream instructions, prefer the Edgar naming (binary: `edgar-cli`, crate: `edgar-cli`) unless explicitly referenced otherwise.

## Merging Upstream Codex Changes

1. `git fetch origin main` to pull the latest upstream Codex history into `origin/main`.
2. Merge into `dev` with `git merge origin/main`.
3. Resolve conflicts while preserving Edgar-specific behavior:
   - Keep the `edgar` branding (binary names, home directory, documentation) unless upstream deliberately changes behavior.
   - Ensure the CLI still builds by keeping the crate named `codex_cli` (library) and package/binary `edgar-cli`.
   - When upstream moves or renames modules (e.g. codex message processor relocation), adopt the upstream structure, then reapply Edgar adjustments.
4. After conflicts are resolved, regenerate the lockfile if needed with `cargo generate-lockfile`.
5. Run formatting and tests: `just fmt`, `just fix`, `cargo test -p edgar-cli`, `cargo test -p codex-core`, `cargo test -p codex-login`, and `cargo test --all-features`.
6. Commit the merge and push.

