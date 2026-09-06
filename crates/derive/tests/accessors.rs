use signalbox_derive::Accessors;
use std::num::NonZeroU64;

#[derive(Accessors)]
struct Record {
    /// The stored label.
    #[get(str)]
    label: String,
    #[get(slice)]
    entries: Vec<u64>,
    #[get(unbox)]
    boxed: Box<u64>,
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
    let record = Record {
        label: "label".to_owned(),
        entries: vec![3, 5],
        boxed: Box::new(7),
        value: 11,
    };
    assert_eq!(record.label(), "label");
    assert_eq!(record.entries(), [3, 5]);
    assert_eq!(*record.boxed(), 7);
    assert_eq!(*record.borrowed(), 11);
}

#[test]
fn consuming_scalar_accessors_are_const() {
    const NUMBER: u64 = Number(NonZeroU64::new(13).unwrap()).get();
    const VALUE: u64 = CopyRecord { value: 17 }.value();
    assert_eq!(NUMBER, 13);
    assert_eq!(VALUE, 17);
}
