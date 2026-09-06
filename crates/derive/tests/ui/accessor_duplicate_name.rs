use signalbox_derive::Accessors;

#[derive(Accessors)]
struct Invalid {
    #[get(as = "value")]
    first: u64,
    #[get(as = "value")]
    second: u64,
}

fn main() {}
