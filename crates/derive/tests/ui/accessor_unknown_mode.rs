use signalbox_derive::Accessors;

#[derive(Accessors)]
struct Invalid {
    #[get(mystery)]
    value: u64,
}

fn main() {}
