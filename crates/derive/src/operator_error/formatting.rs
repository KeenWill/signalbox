use std::collections::{BTreeMap, BTreeSet};

use quote::quote;
use syn::{
    Expr, Ident, Lit, Token, ext::IdentExt, parse::Parser, punctuated::Punctuated, visit::Visit,
};

use super::Field;

pub(super) struct References {
    pub(super) used: BTreeSet<String>,
    pub(super) formatted: BTreeMap<String, BTreeSet<&'static str>>,
    bindings: BTreeMap<String, Ident>,
}

impl References {
    pub(super) fn new(fields: &[Field<'_>]) -> Self {
        Self {
            used: BTreeSet::new(),
            formatted: BTreeMap::new(),
            bindings: fields
                .iter()
                .map(|field| (field.binding.unraw().to_string(), field.binding.clone()))
                .collect(),
        }
    }

    fn field(&self, expression: &Expr) -> Option<String> {
        let name = match expression {
            Expr::Path(path) => path.path.get_ident()?.unraw().to_string(),
            Expr::Reference(reference) => return self.field(&reference.expr),
            Expr::Paren(expression) => return self.field(&expression.expr),
            Expr::Group(expression) => return self.field(&expression.expr),
            Expr::Field(field) if matches!(field.base.as_ref(), Expr::Path(path) if path.path.is_ident("self")) => {
                match &field.member {
                    syn::Member::Named(name) => name.unraw().to_string(),
                    syn::Member::Unnamed(index) => format!("field_{}", index.index),
                }
            }
            _ => return None,
        };
        self.bindings.get(&name).map(ToString::to_string)
    }

    fn formatted(&mut self, expression: &Expr, mode: &'static str) {
        if let Some(field) = self.field(expression) {
            self.formatted.entry(field).or_default().insert(mode);
        }
    }

    fn implicit(&mut self, name: &str, named: &BTreeMap<String, &Expr>) {
        if !named.contains_key(name)
            && let Some(binding) = self.bindings.get(name)
        {
            self.used.insert(binding.to_string());
        }
    }

    fn format_arguments(&mut self, name: &str, args: &Punctuated<Expr, Token![,]>) {
        let offset = match name {
            "write" | "writeln" => 1,
            "format" | "format_args" | "format_args_nl" | "print" | "println" | "eprint"
            | "eprintln" => 0,
            _ => return,
        };
        let Some(Expr::Lit(syn::ExprLit {
            lit: Lit::Str(literal),
            ..
        })) = args.get(offset)
        else {
            return;
        };
        let mut named = BTreeMap::new();
        let mut positional = Vec::new();
        for argument in args.iter().skip(offset + 1) {
            if let Expr::Assign(assignment) = argument
                && let Expr::Path(path) = assignment.left.as_ref()
                && let Some(name) = path.path.get_ident()
            {
                named.insert(name.unraw().to_string(), assignment.right.as_ref());
            } else {
                positional.push(argument);
            }
        }
        let mut next = 0;
        for (key, specification) in placeholders(&literal.value()) {
            if specification.contains(".*") {
                next += 1;
            }
            let argument = if key.is_empty() {
                let argument = positional.get(next).copied();
                next += 1;
                argument
            } else if let Ok(index) = key.parse::<usize>() {
                positional.get(index).copied()
            } else {
                self.implicit(&key, &named);
                named.get(&key).copied()
            };
            let mode = match specification.trim_end().chars().last() {
                Some('?') => "Debug",
                Some('x') => "LowerHex",
                Some('X') => "UpperHex",
                Some('b') => "Binary",
                Some('o') => "Octal",
                Some('p') => "Pointer",
                Some('e') => "LowerExp",
                Some('E') => "UpperExp",
                _ => "Display",
            };
            if let Some(argument) = argument {
                self.formatted(argument, mode);
            } else if let Some(binding) = self.bindings.get(&key) {
                self.formatted
                    .entry(binding.to_string())
                    .or_default()
                    .insert(mode);
            }
            for (end, ch) in specification.char_indices() {
                if ch != '$' {
                    continue;
                }
                let start = specification[..end]
                    .char_indices()
                    .rfind(|(_, ch)| !ch.is_alphanumeric() && *ch != '_')
                    .map_or(0, |(index, ch)| index + ch.len_utf8());
                let name = &specification[start..end];
                self.implicit(name, &named);
                if let Some(name) = name.strip_prefix('0') {
                    self.implicit(name, &named);
                }
            }
        }
    }

    fn callee(&mut self, expression: &Expr) {
        match expression {
            Expr::Path(_) => {}
            Expr::Paren(expression) => self.callee(&expression.expr),
            Expr::Group(expression) => self.callee(&expression.expr),
            expression => self.visit_expr(expression),
        }
    }
}

impl<'ast> Visit<'ast> for References {
    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if let Some(ident) = expression.path.get_ident() {
            self.used.insert(ident.to_string());
        }
    }

    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if let Expr::Path(path) = expression.func.as_ref()
            && path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .eq(["std", "fmt", "Display", "fmt"])
            && let Some(argument) = expression.args.first()
        {
            self.formatted(argument, "Display");
        }
        self.callee(&expression.func);
        for argument in &expression.args {
            self.visit_expr(argument);
        }
    }

    fn visit_expr_assign(&mut self, expression: &'ast syn::ExprAssign) {
        self.visit_expr(&expression.right);
    }

    fn visit_macro(&mut self, expression: &'ast syn::Macro) {
        if let Ok(arguments) =
            Punctuated::<Expr, Token![,]>::parse_terminated.parse2(expression.tokens.clone())
        {
            if let Some(segment) = expression.path.segments.last() {
                self.format_arguments(&segment.ident.to_string(), &arguments);
            }
            for argument in &arguments {
                self.visit_expr(argument);
            }
        } else {
            let tokens = &expression.tokens;
            if let Ok(repeat) = syn::parse2::<syn::ExprRepeat>(quote!([#tokens])) {
                self.visit_expr_repeat(&repeat);
            } else {
                super::identifiers(tokens.clone(), &mut self.used);
            }
        }
    }
}

fn placeholders(source: &str) -> Vec<(String, String)> {
    let mut chars = source.chars().peekable();
    let mut result = Vec::new();
    while let Some(ch) = chars.next() {
        if ch != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        let mut placeholder = String::new();
        for ch in chars.by_ref() {
            if ch == '}' {
                break;
            }
            placeholder.push(ch);
        }
        let (key, specification) = placeholder.split_once(':').unwrap_or((&placeholder, ""));
        result.push((key.trim().to_owned(), specification.to_owned()));
    }
    result
}
