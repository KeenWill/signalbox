{ ... }:

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
  languages.python = {
    enable = true;
    venv = {
      enable = true;
      requirements = ./tooling/requirements-mdformat.txt;
    };
  };
}
