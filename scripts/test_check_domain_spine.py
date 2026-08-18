#!/usr/bin/env python3
"""Prove the domain-spine checker sees method-surface drift on listed types."""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_domain_spine.py"

DOMAIN_LIB = """\
//! Fixture domain crate root.

mod widget;

pub use widget::{Tag, Widget, WidgetError, widget_count};

define_identity!(
    /// One fixture session.
    SessionId
);
"""

WIDGET_BASELINE = """\
//! Fixture widget module.
//!
//! ```
//! impl QuotedInDocs {
//!     pub fn never_counted(&self) {}
//! }
//! ```

macro_rules! bounded_text {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        pub struct $name(String);

        impl $name {
            pub fn try_new(value: String) -> Result<Self, WidgetError> {
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }
    };
}

bounded_text!(
    /// One fixture bounded text.
    Tag
);

#[cfg(all(
    test,
    any(target_os = "linux", target_os = "macos")
))]
impl Widget {
    pub fn multiline_cfg_helper(&self) -> u64 {
        self.value
    }
}

/// One fixture public type.
pub struct Widget {
    value: u64,
}

impl Widget {
    /// Builds one widget.
    pub fn new(value: u64) -> Self {
        Self { value }
    }

    /// Returns the stored value.
    pub const fn value(&self) -> u64 {
        self.value
    }

    /// Returns the fixture kind.
    pub const fn r#type(&self) -> u64 {
        0
    }

    #[cfg(test)]
    pub fn body_test_helper(&self) -> u64 {
        self.value
    }

    fn double(&self) -> u64 {
        self.value * 2
    }
}

#[cfg(test)]
mod helpers;

/// One fixture failure type.
pub struct WidgetError;

/// Counts fixture widgets.
pub fn widget_count() -> usize {
    0
}

struct Hidden;

impl Hidden {
    pub fn reveal(&self) -> &'static str {
        "{ not a block opener }"
    }
}

/* A nested block comment stays a comment past its inner close:
/* inner */
impl Widget {
    pub fn commented_out(&self) {}
}
*/

#[cfg(test)]
#[allow(
    clippy::unused_self,
    reason = "fixture mirrors a multi-line attribute between cfg and item"
)]
impl Widget {
    pub fn test_only_helper(&self) -> u64 {
        self.value
    }
}

#[cfg(test)]
mod tests {
    impl super::Widget {
        pub fn tests_module_helper(&self) -> u64 {
            self.value
        }
    }
}

#[cfg(all(test, unix))]
impl Widget {
    pub fn composite_test_helper(&self) -> u64 {
        self.value
    }
}

impl<Part> Blueprint<Part> {
    pub fn render(&self) -> &'static str {
        "unlisted generic type"
    }
}

struct Blueprint<Part> {
    part: Part,
}
"""

APPLICATION_LIB = """\
//! Fixture application crate root.

mod service;

pub use service::Service;
"""

SERVICE_BASELINE = """\
//! Fixture service module.

/// One fixture service.
pub struct Service;

impl Service {
    /// Runs the fixture service.
    pub async fn run(&self) -> bool {
        true
    }
}
"""

SPINE_BASELINE = """\
# Domain spine

Fixture spine for the checker's self-test.

## domain: lib.rs — identities

```rust
pub struct SessionId(/* private */);
```

## domain: widget

```rust
pub struct Widget { /* private */ }
impl Widget {
    pub fn new(value: u64) -> Self;
    pub const fn r#type(&self) -> u64;
    // accessor: value()
}

pub struct WidgetError;

pub struct Tag(/* private */);
impl Tag {
    pub fn try_new(value: String) -> Result<Self, WidgetError>;
    // accessors: as_str(), into_string()
}

pub fn widget_count() -> usize;
```

## application: service

```rust
pub struct Service;
impl Service {
    pub async fn run(&self) -> bool;
}
```

## Inventory

| Module | Exports | Notes |
| --- | --- | --- |
| domain: lib.rs identities | 1 | |
| domain: widget | 3 (+1 free fn) | |
| application: service | 1 | |
| **signalbox-domain total** | **4 (+1 free fn)** | |
| **signalbox-application total** | **1** | |
"""


def run_checker(root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(CHECKER)],
        cwd=root,
        check=False,
        capture_output=True,
        text=True,
    )


HELPERS_MODULE = """\
//! Fixture helpers loaded by a test-gated out-of-line module.

impl super::Widget {
    pub fn helpers_only(&self) -> u64 {
        self.value
    }
}
"""


def write_fixture(
    root: Path,
    widget: str = WIDGET_BASELINE,
    service: str = SERVICE_BASELINE,
    spine: str = SPINE_BASELINE,
) -> None:
    domain_src = root / "crates" / "domain" / "src"
    application_src = root / "crates" / "application" / "src"
    domain_src.mkdir(parents=True, exist_ok=True)
    application_src.mkdir(parents=True, exist_ok=True)
    (root / "docs").mkdir(exist_ok=True)
    (domain_src / "lib.rs").write_text(DOMAIN_LIB, encoding="utf-8")
    (domain_src / "widget.rs").write_text(widget, encoding="utf-8")
    (domain_src / "widget").mkdir(exist_ok=True)
    (domain_src / "widget" / "helpers.rs").write_text(
        HELPERS_MODULE, encoding="utf-8"
    )
    (application_src / "lib.rs").write_text(APPLICATION_LIB, encoding="utf-8")
    (application_src / "service.rs").write_text(service, encoding="utf-8")
    (root / "docs" / "domain-spine.md").write_text(spine, encoding="utf-8")


def expect_pass(result: subprocess.CompletedProcess[str], case: str) -> None:
    assert result.returncode == 0, (
        f"{case}: expected pass, got:\n{result.stdout}{result.stderr}"
    )


def expect_failure(
    result: subprocess.CompletedProcess[str], case: str, fragment: str
) -> None:
    assert result.returncode == 1, (
        f"{case}: expected failure, got rc {result.returncode}:\n"
        f"{result.stdout}{result.stderr}"
    )
    assert fragment in result.stdout, (
        f"{case}: diagnostics lack {fragment!r}:\n{result.stdout}"
    )


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="signalbox-domain-spine-") as directory:
        root = Path(directory)

        # The baseline agrees everywhere — including a method-level
        # `#[cfg(test)] pub fn` inside a production impl, a rustfmt-style
        # multi-line cfg predicate on a test-only impl, a test-gated
        # out-of-line `mod helpers;` whose file impls the listed type, and
        # a macro-minted Tag validated against its expansion contract:
        # accessor-comment declarations count, a doc-comment impl example, a
        # nested-block-comment impl, test-configured impls and modules
        # (including a multi-line attribute between the cfg and its item),
        # and public methods on unlisted types (Hidden, generic Blueprint)
        # are ignored, and the macro-surfaced Tag keeps its declared methods
        # without a textual impl.
        write_fixture(root)
        expect_pass(run_checker(root), "baseline")

        # A method going public on a listed type without a spine declaration
        # is the drift this check exists to catch.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "    fn double(&self) -> u64 {",
                "    pub fn is_open(&self) -> bool {\n"
                "        true\n"
                "    }\n"
                "\n"
                "    fn double(&self) -> u64 {",
            ),
        )
        expect_failure(
            run_checker(root),
            "undeclared public method",
            "domain::widget::Widget has public method is_open() with no"
            " declaration in the 'domain: widget' section",
        )

        # A declared method the source no longer defines is stale the other
        # way around.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "    /// Returns the stored value.\n"
                "    pub const fn value(&self) -> u64 {\n"
                "        self.value\n"
                "    }\n\n",
                "",
            ),
        )
        expect_failure(
            run_checker(root),
            "stale declaration",
            "'domain: widget' section declares Widget::value(), which source"
            " no longer defines as a public method",
        )

        # The stale exemption is invocation-driven: with the bounded_text!
        # invocation gone, Tag's declared methods have no producer left.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "bounded_text!(\n"
                "    /// One fixture bounded text.\n"
                "    Tag\n"
                ");\n\n",
                "/// One fixture bounded text.\npub struct Tag(String);\n\n",
            ),
        )
        expect_failure(
            run_checker(root),
            "macro exemption is invocation-driven",
            "'domain: widget' section declares Tag::try_new(), which source"
            " no longer defines as a public method",
        )

        # A commented-out minting invocation grants nothing: the macro scan
        # runs over comment-blanked text.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "bounded_text!(\n"
                "    /// One fixture bounded text.\n"
                "    Tag\n"
                ");\n",
                "/*\nbounded_text!(\n"
                "    Tag\n"
                ");\n*/\n",
            ),
        )
        expect_failure(
            run_checker(root),
            "commented-out invocation earns no exemption",
            "'domain: widget' section declares Tag::try_new(), which source"
            " no longer defines as a public method",
        )

        # `any(test, feature = ...)` holds when the feature is enabled, so
        # the impl is library surface and its methods stay required.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "#[cfg(test)]\n#[allow(",
                '#[cfg(any(test, feature = "fixture-support"))]\n'
                "impl Widget {\n"
                "    pub fn feature_visible(&self) -> u64 {\n"
                "        self.value\n"
                "    }\n"
                "}\n\n"
                "#[cfg(test)]\n#[allow(",
            ),
        )
        expect_failure(
            run_checker(root),
            "any(test, feature) impl stays public surface",
            "domain::widget::Widget has public method feature_visible() with"
            " no declaration in the 'domain: widget' section",
        )

        # A production inline module's impl is public surface: exclusion is
        # by configuration, not indentation.
        write_fixture(
            root,
            widget=WIDGET_BASELINE
            + "\nmod nested {\n"
            "    impl super::Widget {\n"
            "        pub fn nested_method(&self) -> u64 {\n"
            "            self.value\n"
            "        }\n"
            "    }\n"
            "}\n",
        )
        expect_failure(
            run_checker(root),
            "production inline module is scanned",
            "domain::widget::Widget has public method nested_method() with no"
            " declaration in the 'domain: widget' section",
        )

        # A macro-minted type faces the forward direction too: dropping a
        # generated method's declaration is drift, per the expansion
        # contract extracted from the macro_rules body.
        write_fixture(
            root,
            spine=SPINE_BASELINE.replace(
                "    // accessors: as_str(), into_string()\n",
                "    // accessor: as_str()\n",
            ),
        )
        expect_failure(
            run_checker(root),
            "macro-minted type checked forward",
            "domain::widget::Tag has public method into_string() with no"
            " declaration in the 'domain: widget' section",
        )

        # A where-bound const block is header, not body: the real body
        # behind it is still scanned.
        write_fixture(
            root,
            widget=WIDGET_BASELINE
            + "\nimpl Widget\n"
            "where\n"
            "    [(); { 1 + 1 }]: Sized,\n"
            "{\n"
            "    pub fn where_gated(&self) -> u64 {\n"
            "        2\n"
            "    }\n"
            "}\n",
        )
        expect_failure(
            run_checker(root),
            "where-clause const block is not the body opener",
            "domain::widget::Widget has public method where_gated() with no"
            " declaration in the 'domain: widget' section",
        )

        # A module declared under mutually exclusive cfgs is loaded by
        # library builds; the production declaration wins over the gate.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "#[cfg(test)]\nmod helpers;",
                "#[cfg(test)]\nmod helpers;\n#[cfg(not(test))]\nmod helpers;",
            ),
        )
        expect_failure(
            run_checker(root),
            "production-reachable module stays scanned",
            "domain::widget::Widget has public method helpers_only() with no"
            " declaration in the 'domain: widget' section",
        )

        # Dropping the cfg from the out-of-line module declaration makes
        # the helpers file library surface again.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "#[cfg(test)]\nmod helpers;", "mod helpers;"
            ),
        )
        expect_failure(
            run_checker(root),
            "ungated out-of-line module is scanned",
            "domain::widget::Widget has public method helpers_only() with no"
            " declaration in the 'domain: widget' section",
        )

        # An impl inside an anonymous const's initializer attaches methods
        # crate-wide; the block is a transparent item container.
        write_fixture(
            root,
            widget=WIDGET_BASELINE
            + "\nconst _: () = {\n"
            "    impl Widget {\n"
            "        pub fn from_const_block(&self) -> u64 {\n"
            "            self.value\n"
            "        }\n"
            "    }\n"
            "};\n",
        )
        expect_failure(
            run_checker(root),
            "anonymous-const impl body is scanned",
            "domain::widget::Widget has public method from_const_block() with"
            " no declaration in the 'domain: widget' section",
        )

        # `#[cfg(not(test))]` is library surface, not a test fixture.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "#[cfg(test)]\n#[allow(",
                "#[cfg(not(test))]\nimpl Widget {\n"
                "    pub fn prod_only(&self) -> u64 {\n"
                "        self.value\n"
                "    }\n"
                "}\n\n"
                "#[cfg(test)]\n#[allow(",
            ),
        )
        expect_failure(
            run_checker(root),
            "cfg(not(test)) impl stays public surface",
            "domain::widget::Widget has public method prod_only() with no"
            " declaration in the 'domain: widget' section",
        )

        # A raw-identifier method compares by its unprefixed name, so a
        # source rename under `r#` is still drift.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "    pub const fn r#type(&self) -> u64 {",
                "    pub const fn r#match(&self) -> u64 {",
            ),
        )
        renamed = run_checker(root)
        expect_failure(
            renamed,
            "raw-identifier rename is drift",
            "domain::widget::Widget has public method match() with no"
            " declaration in the 'domain: widget' section",
        )
        assert (
            "'domain: widget' section declares Widget::type(), which source"
            " no longer defines as a public method" in renamed.stdout
        ), f"raw-identifier rename lacks the stale side:\n{renamed.stdout}"

        # Declaring the same method twice is rejected, matching the
        # duplicate-type rule.
        write_fixture(
            root,
            spine=SPINE_BASELINE.replace(
                "    pub fn new(value: u64) -> Self;\n",
                "    pub fn new(value: u64) -> Self;\n"
                "    pub fn new(value: u64) -> Self;\n",
            ),
        )
        expect_failure(
            run_checker(root),
            "duplicate method declaration",
            "'domain: widget' section declares Widget::new() more than once",
        )

        # Only the method-minting macros exempt: an assertion naming the
        # type mints nothing and rescues nothing.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "bounded_text!(\n"
                "    /// One fixture bounded text.\n"
                "    Tag\n"
                ");\n",
                "assert_fixture_contract!(\n"
                "    /// One fixture bounded text.\n"
                "    Tag\n"
                ");\n",
            ),
        )
        expect_failure(
            run_checker(root),
            "non-minting macro earns no exemption",
            "'domain: widget' section declares Tag::try_new(), which source"
            " no longer defines as a public method",
        )

        # An ABI-qualified method is public surface like any other.
        write_fixture(
            root,
            widget=WIDGET_BASELINE.replace(
                "    fn double(&self) -> u64 {",
                '    pub extern "C" fn raw_value(&self) -> u64 {\n'
                "        self.value\n"
                "    }\n"
                "\n"
                "    fn double(&self) -> u64 {",
            ),
        )
        expect_failure(
            run_checker(root),
            "extern method surfaces",
            "domain::widget::Widget has public method raw_value() with no"
            " declaration in the 'domain: widget' section",
        )

        # A const-generic brace in the impl header is not the body opener,
        # and a comparison inside it is not an angle delimiter; the body
        # behind it is still scanned.
        write_fixture(
            root,
            widget=WIDGET_BASELINE
            + "\nimpl Widget<{ 1 < 2 }> {\n"
            "    pub fn shadow(&self) -> u64 {\n"
            "        2\n"
            "    }\n"
            "}\n",
        )
        expect_failure(
            run_checker(root),
            "const-generic impl body is scanned",
            "domain::widget::Widget has public method shadow() with no"
            " declaration in the 'domain: widget' section",
        )

        # Methods declared in a section that does not export the type are
        # misplaced, not silently credited.
        write_fixture(
            root,
            spine=SPINE_BASELINE.replace(
                "pub struct Service;\nimpl Service {",
                "pub struct Service;\nimpl Widget {\n"
                "    pub fn new(value: u64) -> Self;\n"
                "}\nimpl Service {",
            ),
        )
        expect_failure(
            run_checker(root),
            "misplaced method declaration",
            "'application: service' section declares methods for Widget,"
            " which is not part of that module's export surface",
        )

        # An impl header the scan cannot parse fails loudly instead of
        # thinning the measured surface.
        write_fixture(
            root,
            widget=WIDGET_BASELINE + "\nimpl <Widget> {\n}\n",
        )
        expect_failure(
            run_checker(root),
            "unparseable impl header",
            "has an impl header this check cannot parse",
        )

        # An async method surfaces like any other; removing its declaration
        # fails in the application crate too.
        write_fixture(
            root,
            spine=SPINE_BASELINE.replace(
                "    pub async fn run(&self) -> bool;\n", ""
            ),
        )
        expect_failure(
            run_checker(root),
            "application crate coverage",
            "application::service::Service has public method run() with no"
            " declaration in the 'application: service' section",
        )
    print("domain-spine checker self-test passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
