use std::{error::Error, fmt};

use signalbox_derive::OperatorError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperatorFailureClass {
    CallerOrHubBug,
    Retryable,
}

trait ClassifyOperatorFailure {
    fn operator_failure_class(&self) -> OperatorFailureClass;
    fn operator_failure_cause_code(&self) -> &'static str;
}

#[derive(Debug, OperatorError)]
#[error("leaf failure")]
#[operator(class = Retryable, code = "leaf")]
struct Leaf;

#[derive(Debug, OperatorError)]
#[error("{0}")]
#[operator(class = Retryable, code = "detail")]
struct Detail(&'static str);

#[derive(Debug, OperatorError)]
#[operator(default_class = CallerOrHubBug)]
enum Service<T> {
    #[error("load failed: {0}")]
    #[operator(delegate, code = "load")]
    Load(#[source] T),
    #[error("executor failed ({executor}); classification failed: {classification}")]
    #[operator(delegate = classification, code = delegate)]
    Classify {
        executor: T,
        #[source]
        classification: T,
    },
    #[error("recovered failure")]
    #[operator(class = failure_class, code = cause_code)]
    Recovered {
        failure_class: OperatorFailureClass,
        cause_code: &'static str,
    },
    #[error("invalid request")]
    #[operator(code = "invalid")]
    Invalid,
}

#[derive(Debug, OperatorError)]
enum NoSource<T> {
    #[error("conversion failed: {0}")]
    Conversion(T),
}

struct DisplayOnly;

impl fmt::Display for DisplayOnly {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("display only")
    }
}

#[derive(Debug, OperatorError)]
#[error(transparent)]
struct Transparent(Leaf);

#[derive(Debug, OperatorError)]
#[error("repeated concern `{}`", concern.as_str())]
struct Concern {
    concern: String,
}

#[test]
fn error_roles_preserve_their_independent_field_selection() {
    let load = Service::Load(Leaf);
    assert_eq!(load.to_string(), "load failed: leaf failure");
    assert_eq!(
        load.source().map(ToString::to_string),
        Some("leaf failure".to_owned())
    );
    assert_eq!(
        load.operator_failure_class(),
        OperatorFailureClass::Retryable
    );
    assert_eq!(load.operator_failure_cause_code(), "load");

    let classification = Service::Classify {
        executor: Detail("executor detail"),
        classification: Detail("storage detail"),
    };
    assert_eq!(
        classification.to_string(),
        "executor failed (executor detail); classification failed: storage detail"
    );
    assert_eq!(
        classification.source().map(ToString::to_string),
        Some("storage detail".to_owned())
    );
    assert_eq!(classification.operator_failure_cause_code(), "detail");
}

#[test]
fn constant_and_carried_classification_keep_source_absent() {
    let recovered = Service::<Leaf>::Recovered {
        failure_class: OperatorFailureClass::Retryable,
        cause_code: "original",
    };
    assert_eq!(recovered.to_string(), "recovered failure");
    assert_eq!(
        recovered.operator_failure_class(),
        OperatorFailureClass::Retryable
    );
    assert_eq!(recovered.operator_failure_cause_code(), "original");
    assert!(recovered.source().is_none());
    let invalid = Service::<Leaf>::Invalid;
    assert_eq!(
        invalid.operator_failure_class(),
        OperatorFailureClass::CallerOrHubBug
    );
    assert_eq!(invalid.operator_failure_cause_code(), "invalid");
    assert!(invalid.source().is_none());
}

#[test]
fn display_does_not_require_error_or_classification_bounds() {
    assert_eq!(
        NoSource::Conversion(DisplayOnly).to_string(),
        "conversion failed: display only"
    );
    assert!(NoSource::Conversion(Leaf).source().is_none());
}

#[test]
fn transparent_display_and_format_arguments_preserve_text_without_adding_sources() {
    assert_eq!(Transparent(Leaf).to_string(), "leaf failure");
    assert!(Transparent(Leaf).source().is_none());
    assert_eq!(
        Concern {
            concern: "correctness".to_owned()
        }
        .to_string(),
        "repeated concern `correctness`"
    );
}

#[derive(Debug, OperatorError)]
#[error("cause: {cause}")]
#[operator(
    display_bound = "T: fmt::Display",
    error_bound = "T: Error + 'static, U: fmt::Debug",
    classify_bound = "T: ClassifyOperatorFailure",
    delegate = cause,
    code = delegate
)]
struct ExplicitBounds<T, U> {
    #[source]
    cause: T,
    marker: std::marker::PhantomData<U>,
}

#[derive(Debug)]
struct OnlyDebug;

#[test]
fn explicit_bounds_do_not_constrain_unrelated_type_parameters() {
    let failure = ExplicitBounds::<Leaf, OnlyDebug> {
        cause: Leaf,
        marker: std::marker::PhantomData,
    };
    expect_test::expect![["cause: leaf failure"]].assert_eq(&failure.to_string());
    assert!(failure.source().is_some());
    assert_eq!(
        failure.operator_failure_class(),
        OperatorFailureClass::Retryable
    );
    assert_eq!(failure.operator_failure_cause_code(), "leaf");
}

#[derive(Debug)]
struct Padded;

impl fmt::Display for Padded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad("detail")
    }
}

impl Error for Padded {}

#[derive(Debug, OperatorError)]
#[error(transparent)]
struct Annotated {
    #[source]
    failure: Padded,
    _context: usize,
}

#[test]
fn transparent_metadata_wrapper_preserves_formatter_precision() {
    let failure = Annotated {
        failure: Padded,
        _context: 1,
    };
    assert_eq!(format!("{failure:.3}"), "det");
    assert_eq!(
        failure.source().map(ToString::to_string),
        Some("detail".to_owned())
    );
}
