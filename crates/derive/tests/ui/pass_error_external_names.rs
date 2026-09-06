#![deny(warnings)]

use signalbox_derive::OperatorError;

fn render(value: &str) -> &str {
    value
}

mod path {
    pub fn render(value: &str) -> &str {
        value
    }
}

#[allow(non_snake_case)]
#[derive(Debug, OperatorError)]
#[error("{} {} {} {} {:?}", render(value), path::render(value), value.len(), String::from(value.as_str()), vec![render(value); 2])]
struct ExternalNames {
    value: String,
    render: bool,
    path: bool,
    len: bool,
    String: bool,
}

fn main() {
    let failure = ExternalNames {
        value: "text".to_owned(),
        render: false,
        path: false,
        len: false,
        String: false,
    };
    assert_eq!(failure.to_string(), "text text 4 text [\"text\", \"text\"]");
    assert!(!failure.render && !failure.path && !failure.len && !failure.String);
}
