use signalbox_derive::Accessors;
use std::{num::NonZeroU64, sync::Arc};

#[derive(Accessors)]
struct Record {
    /// The stored label.
    #[get(str, into)]
    label: String,
    #[get(slice)]
    entries: Vec<u64>,
    #[get(opt_ref)]
    optional: Option<String>,
    #[get(unbox)]
    boxed: Box<u64>,
    #[get(clone)]
    shared: Arc<String>,
    #[get(as = "borrowed")]
    value: u64,
}

#[derive(Clone, Copy, Accessors)]
struct Number(#[get(inner, as = "get")] NonZeroU64);

#[derive(Clone, Copy, Accessors)]
struct CopyRecord {
    #[get(copy)]
    value: u64,
}

#[test]
fn borrowing_modes_preserve_values_without_moving_the_record() {
    let shared = Arc::new("shared".to_owned());
    let record = Record {
        label: "label".to_owned(),
        entries: vec![3, 5],
        optional: Some("optional".to_owned()),
        boxed: Box::new(7),
        shared: shared.clone(),
        value: 11,
    };
    assert_eq!(record.label(), "label");
    assert_eq!(record.entries(), [3, 5]);
    assert_eq!(record.optional().map(String::as_str), Some("optional"));
    assert_eq!(*record.boxed(), 7);
    assert!(Arc::ptr_eq(&record.shared(), &shared));
    assert_eq!(*record.borrowed(), 11);
    assert_eq!(record.into_label(), "label");
}

#[test]
fn consuming_scalar_accessors_are_const() {
    const NUMBER: u64 = Number(NonZeroU64::new(13).unwrap()).get();
    const VALUE: u64 = CopyRecord { value: 17 }.value();
    assert_eq!(NUMBER, 13);
    assert_eq!(VALUE, 17);
}

#[derive(Accessors)]
struct Generic<T> {
    #[get(clone)]
    value: T,
}

#[test]
fn clone_bounds_apply_to_the_method() {
    struct NotClone;
    let _ = Generic { value: NotClone };
    assert_eq!(
        Generic {
            value: "copy".to_owned()
        }
        .value(),
        "copy"
    );
}
