use signalbox_derive::Accessors;

#[derive(Accessors)]
struct Invalid {
    #[get(slice)]
    value: u64,
}

fn main() {}
