"""Run the declared Markdown formatter as a Bazel test."""

import runpy

if __name__ == "__main__":
    runpy.run_module("mdformat", run_name="__main__", alter_sys=True)
