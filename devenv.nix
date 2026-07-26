{ pkgs, ... }:

{
  # Developer environment for repository tooling. Enter with `devenv shell`,
  # or run a single command with `devenv shell -- <command> <args>`; direnv
  # users get it automatically after a one-time `direnv allow` (see .envrc).
  #
  # The Markdown toolchain is installed from the same fully frozen pin file
  # CI uses (tooling/requirements-mdformat.txt), so local mdformat output is
  # byte-identical to CI. Never run a system or Homebrew mdformat against
  # this repository: without the GFM plugin it silently corrupts GFM tables
  # under .mdformat.toml's wrap=80.

  # Shared compiler cache: sccache wraps rustc for cargo invocations inside
  # this environment, so dependency compilation is cached once per machine
  # and reused across checkouts and worktrees. The cache lives in sccache's
  # per-user default location (override with SCCACHE_DIR); workspace crates
  # keep incremental compilation and are passed through uncached. CI never
  # enters this environment — its caching is configured in
  # .github/workflows/rust.yml.
  packages = [ pkgs.sccache ];
  env.RUSTC_WRAPPER = "sccache";

  languages.python = {
    enable = true;
    venv = {
      enable = true;
      requirements = ./tooling/requirements-mdformat.txt;
    };
  };
}
