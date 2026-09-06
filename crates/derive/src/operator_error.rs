use std::collections::BTreeSet;

use proc_macro2::{TokenStream, TokenTree};
use quote::{format_ident, quote};
use syn::{
    Attribute, Data, DeriveInput, Expr, Fields, Generics, Ident, Lit, LitStr, Member, Token,
    TypeParamBound, WhereClause, ext::IdentExt, parse_quote, punctuated::Punctuated,
    spanned::Spanned, visit::Visit,
};

#[derive(Default)]
struct Options {
    default_class: Option<Expr>,
    display_bound: Option<LitStr>,
    error_bound: Option<LitStr>,
    classify_bound: Option<LitStr>,
    classify: bool,
}

#[derive(Default)]
struct Classification {
    delegate: bool,
    target: Option<Member>,
    class: Option<Expr>,
    code: Option<Expr>,
}

struct Field<'a> {
    member: Member,
    binding: Ident,
    source: bool,
    field: &'a syn::Field,
}

fn fields(fields: &Fields) -> Vec<Field<'_>> {
    fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let member = field
                .ident
                .clone()
                .map(Member::Named)
                .unwrap_or_else(|| Member::Unnamed(syn::Index::from(index)));
            let binding = field
                .ident
                .clone()
                .unwrap_or_else(|| format_ident!("field_{index}", span = field.span()));
            Field {
                member,
                binding,
                source: field.attrs.iter().any(|a| a.path().is_ident("source")),
                field,
            }
        })
        .collect()
}

fn classification(attrs: &[Attribute]) -> syn::Result<Classification> {
    let mut result = Classification::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("operator")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("delegate") {
                result.delegate = true;
                if meta.input.peek(Token![=]) {
                    result.target = Some(meta.value()?.parse()?);
                }
            } else if meta.path.is_ident("class") {
                result.class = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("code") {
                result.code = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("default_class")
                || meta.path.is_ident("display_bound")
                || meta.path.is_ident("error_bound")
                || meta.path.is_ident("classify_bound")
            {
                let _: Expr = meta.value()?.parse()?;
            } else {
                return Err(meta.error("unknown operator variant option"));
            }
            Ok(())
        })?;
    }
    Ok(result)
}

fn options(attrs: &[Attribute]) -> syn::Result<Options> {
    let mut result = Options::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("operator")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("default_class") {
                result.default_class = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("display_bound") {
                result.display_bound = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("error_bound") {
                result.error_bound = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("classify_bound") {
                result.classify_bound = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("class") || meta.path.is_ident("code") {
                result.classify = true;
                let _: Expr = meta.value()?.parse()?;
            } else if meta.path.is_ident("delegate") {
                result.classify = true;
                if meta.input.peek(Token![=]) {
                    let _: Member = meta.value()?.parse()?;
                }
            } else {
                return Err(meta.error("unknown operator type option"));
            }
            Ok(())
        })?;
    }
    Ok(result)
}

fn bounded(
    input: &Generics,
    explicit: Option<&LitStr>,
    bounds: Punctuated<TypeParamBound, Token![+]>,
) -> syn::Result<Generics> {
    let mut generics = input.clone();
    if let Some(explicit) = explicit {
        if !explicit.value().trim().is_empty() {
            let clause = syn::parse_str::<WhereClause>(&format!("where {}", explicit.value()))
                .map_err(|error| syn::Error::new_spanned(explicit, error))?;
            generics
                .make_where_clause()
                .predicates
                .extend(clause.predicates);
        }
    } else {
        for param in input.type_params() {
            let name = &param.ident;
            generics
                .make_where_clause()
                .predicates
                .push(parse_quote!(#name: #bounds));
        }
    }
    Ok(generics)
}

fn identifiers(tokens: TokenStream, used: &mut BTreeSet<String>) {
    for token in tokens {
        match token {
            TokenTree::Ident(ident) => {
                used.insert(ident.to_string());
            }
            TokenTree::Group(group) => identifiers(group.stream(), used),
            _ => {}
        }
    }
}

fn pattern(
    prefix: &TokenStream,
    shape: &Fields,
    fields: &[Field<'_>],
    used: &BTreeSet<String>,
) -> TokenStream {
    let values = fields.iter().map(|field| {
        let binding = &field.binding;
        if used.contains(&binding.to_string()) {
            quote!(#binding)
        } else {
            quote!(_)
        }
    });
    match shape {
        Fields::Unit => prefix.clone(),
        Fields::Unnamed(_) => quote!(#prefix(#(#values),*)),
        Fields::Named(_) => {
            let entries = fields.iter().map(|field| {
                let binding = &field.binding;
                let member = &field.member;
                if used.contains(&binding.to_string()) {
                    quote!(#binding)
                } else {
                    quote!(#member: _)
                }
            });
            quote!(#prefix { #(#entries),* })
        }
    }
}

#[derive(Default)]
struct References(BTreeSet<String>);

impl References {
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
            self.0.insert(ident.to_string());
        }
    }

    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        self.callee(&expression.func);
        for argument in &expression.args {
            self.visit_expr(argument);
        }
    }

    fn visit_expr_assign(&mut self, expression: &'ast syn::ExprAssign) {
        self.visit_expr(&expression.right);
    }

    fn visit_macro(&mut self, expression: &'ast syn::Macro) {
        use syn::parse::Parser;

        if let Ok(arguments) =
            Punctuated::<Expr, Token![,]>::parse_terminated.parse2(expression.tokens.clone())
        {
            for argument in &arguments {
                self.visit_expr(argument);
            }
        } else {
            let tokens = &expression.tokens;
            if let Ok(repeat) = syn::parse2::<syn::ExprRepeat>(quote!([#tokens])) {
                self.visit_expr_repeat(&repeat);
            } else {
                identifiers(tokens.clone(), &mut self.0);
            }
        }
    }
}

fn arm(
    prefix: &TokenStream,
    shape: &Fields,
    fields: &[Field<'_>],
    body: TokenStream,
) -> syn::Result<TokenStream> {
    let mut references = References::default();
    references.visit_expr(&syn::parse2(body.clone())?);
    let pattern = pattern(prefix, shape, fields, &references.0);
    Ok(quote!(#pattern => #body))
}

fn format_literal(
    literal: &LitStr,
    fields: &[Field<'_>],
    positional: bool,
) -> (LitStr, Vec<Ident>) {
    let source = literal.value();
    let mut chars = source.chars().peekable();
    let mut text = String::new();
    let mut captures = Vec::new();
    while let Some(ch) = chars.next() {
        text.push(ch);
        if ch != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            text.push('{');
            chars.next();
            continue;
        }
        let mut placeholder = String::new();
        while chars.peek().is_some_and(|ch| *ch != '}') {
            if let Some(ch) = chars.next() {
                placeholder.push(ch);
            }
        }
        let split = placeholder.find(':').unwrap_or(placeholder.len());
        text.push_str(&capture(
            &placeholder[..split],
            fields,
            positional,
            &mut captures,
        ));
        let specification = &placeholder[split..];
        let mut start = 0;
        for (end, ch) in specification.char_indices() {
            if ch != '$' {
                continue;
            }
            let key_start = specification[..end]
                .char_indices()
                .rfind(|(_, ch)| !ch.is_alphanumeric() && *ch != '_')
                .map_or(0, |(index, ch)| index + ch.len_utf8());
            let key = &specification[key_start..end];
            // A zero-padding flag precedes a named width without a separator.
            let key_start = if key.starts_with('0')
                && key[1..].starts_with(|ch: char| ch.is_alphabetic() || ch == '_')
            {
                key_start + 1
            } else {
                key_start
            };
            text.push_str(&specification[start..key_start]);
            text.push_str(&capture(
                &specification[key_start..end],
                fields,
                positional,
                &mut captures,
            ));
            text.push('$');
            start = end + 1;
        }
        text.push_str(&specification[start..]);
    }
    (LitStr::new(&text, literal.span()), captures)
}

fn capture(key: &str, fields: &[Field<'_>], positional: bool, captures: &mut Vec<Ident>) -> String {
    let field = fields.iter().find(|field| {
        field.binding.unraw() == key
            || (!positional
                && match &field.member {
                    Member::Unnamed(index) => key == index.index.to_string(),
                    Member::Named(name) => name.unraw() == key,
                })
    });
    if let Some(field) = field {
        if !captures.contains(&field.binding) {
            captures.push(field.binding.clone());
        }
        field.binding.unraw().to_string()
    } else {
        key.to_owned()
    }
}

fn display(
    attrs: &[Attribute],
    fields: &[Field<'_>],
    owner: &TokenStream,
    formatter: &Ident,
) -> syn::Result<TokenStream> {
    let attr = attrs
        .iter()
        .find(|a| a.path().is_ident("error"))
        .ok_or_else(|| syn::Error::new_spanned(owner, "missing #[error]"))?;
    let args = attr.parse_args_with(Punctuated::<Expr, Token![,]>::parse_terminated)?;
    let mut args = args.into_iter();
    let first = args
        .next()
        .ok_or_else(|| syn::Error::new_spanned(attr, "expected error format"))?;
    if matches!(&first, Expr::Path(path) if path.path.is_ident("transparent")) {
        let field = fields
            .first()
            .filter(|_| fields.len() == 1)
            .ok_or_else(|| syn::Error::new_spanned(attr, "transparent requires one field"))?;
        let binding = &field.binding;
        return Ok(quote!(::std::fmt::Display::fmt(#binding, #formatter)));
    }
    let Expr::Lit(syn::ExprLit {
        lit: Lit::Str(literal),
        ..
    }) = first
    else {
        return Err(syn::Error::new_spanned(
            first,
            "expected error format string",
        ));
    };
    let args = args.collect::<Vec<_>>();
    let supplied = args
        .iter()
        .filter_map(|argument| {
            let Expr::Assign(assignment) = argument else {
                return None;
            };
            let Expr::Path(path) = assignment.left.as_ref() else {
                return None;
            };
            path.path.get_ident().map(|ident| ident.unraw().to_string())
        })
        .collect::<BTreeSet<_>>();
    let mut arguments = args.iter().map(|arg| quote!(#arg)).collect::<Vec<_>>();
    let (literal, captures) = format_literal(&literal, fields, !arguments.is_empty());
    arguments.extend(
        captures
            .iter()
            .filter(|binding| !supplied.contains(&binding.unraw().to_string()))
            .map(|binding| quote!(#binding = #binding)),
    );
    Ok(quote!(::std::write!(#formatter, #literal #(, #arguments)*)))
}

fn boxed_trait_object(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else {
        return false;
    };
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return false;
    };
    segment.ident == "Box"
        && matches!(
            arguments.args.first(),
            Some(syn::GenericArgument::Type(syn::Type::TraitObject(_)))
        )
}

fn target<'a>(
    classification: &Classification,
    fields: &'a [Field<'a>],
    owner: &TokenStream,
) -> syn::Result<Option<&'a Ident>> {
    if !classification.delegate && classification.target.is_none() {
        return Ok(None);
    }
    let field = if let Some(member) = &classification.target {
        fields.iter().find(|field| field.member == *member)
    } else {
        fields
            .iter()
            .find(|field| field.source)
            .or_else(|| fields.first().filter(|_| fields.len() == 1))
    };
    field
        .map(|field| Some(&field.binding))
        .ok_or_else(|| syn::Error::new_spanned(owner, "bad delegate target"))
}

fn class_expression(expr: &Expr, fields: &[Field<'_>]) -> TokenStream {
    if let Expr::Path(path) = expr
        && let Some(ident) = path.path.get_ident()
    {
        if fields.iter().any(|field| field.binding == *ident) {
            return quote!(*#ident);
        }
        return quote!(OperatorFailureClass::#ident);
    }
    quote!(#expr)
}

pub(super) fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let options = options(&input.attrs)?;
    let mut cases = Vec::new();
    let name = &input.ident;
    match &input.data {
        Data::Enum(data) => {
            for variant in &data.variants {
                let name = &variant.ident;
                cases.push((
                    &variant.attrs,
                    &variant.fields,
                    quote!(Self::#name),
                    quote!(#name),
                ));
            }
        }
        Data::Struct(data) => cases.push((&input.attrs, &data.fields, quote!(Self), quote!(#name))),
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &input,
                "OperatorError requires an enum or struct",
            ));
        }
    }
    let classify = options.classify
        || options.default_class.is_some()
        || cases
            .iter()
            .any(|(attrs, _, _, _)| attrs.iter().any(|a| a.path().is_ident("operator")))
            && matches!(input.data, Data::Enum(_));
    let mut displays = Vec::new();
    let mut names = BTreeSet::new();
    identifiers(quote!(#input), &mut names);
    let mut formatter_name = "__signalbox_formatter".to_owned();
    while names
        .iter()
        .any(|name| name.trim_start_matches("r#") == formatter_name)
    {
        formatter_name.push('_');
    }
    let formatter = Ident::new(&formatter_name, name.span());
    let mut sources = Vec::new();
    let mut classes = Vec::new();
    let mut codes = Vec::new();
    for (attrs, shape, prefix, owner) in cases {
        let fields = fields(shape);
        displays.push(arm(
            &prefix,
            shape,
            &fields,
            display(attrs, &fields, &owner, &formatter)?,
        )?);
        let source_fields = fields
            .iter()
            .filter(|field| field.source)
            .collect::<Vec<_>>();
        let source = match source_fields.as_slice() {
            [] => quote!(::std::option::Option::None),
            [field] => {
                let binding = &field.binding;
                if boxed_trait_object(&field.field.ty) {
                    quote!(Some(#binding.as_ref()))
                } else {
                    quote!(Some(#binding))
                }
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    source_fields[1].field,
                    "only one source is allowed",
                ));
            }
        };
        sources.push(arm(&prefix, shape, &fields, source)?);
        if classify {
            let classification = classification(attrs)?;
            let target = target(&classification, &fields, &owner)?;
            let class = if let Some(expr) = &classification.class {
                class_expression(expr, &fields)
            } else if let Some(target) = target {
                quote!(ClassifyOperatorFailure::operator_failure_class(#target))
            } else if let Some(expr) = &options.default_class {
                class_expression(expr, &fields)
            } else {
                return Err(syn::Error::new_spanned(&owner, "missing operator class"));
            };
            let code = classification
                .code
                .as_ref()
                .ok_or_else(|| syn::Error::new_spanned(&owner, "missing operator code"))?;
            let code = if matches!(code, Expr::Path(path) if path.path.is_ident("delegate")) {
                let target =
                    target.ok_or_else(|| syn::Error::new_spanned(code, "bad delegate target"))?;
                quote!(ClassifyOperatorFailure::operator_failure_cause_code(#target))
            } else {
                quote!(#code)
            };
            classes.push(arm(&prefix, shape, &fields, class)?);
            codes.push(arm(&prefix, shape, &fields, code)?);
        }
    }
    let name = &input.ident;
    let (_, ty_generics, _) = input.generics.split_for_impl();
    let display_generics = bounded(
        &input.generics,
        options.display_bound.as_ref(),
        parse_quote!(::std::fmt::Display),
    )?;
    let (display_impl, _, display_where) = display_generics.split_for_impl();
    let mut output = quote! {
        impl #display_impl ::std::fmt::Display for #name #ty_generics #display_where {
            fn fmt(&self, #formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match self { #(#displays),* }
            }
        }
    };
    {
        let generics = bounded(
            &input.generics,
            options.error_bound.as_ref(),
            parse_quote!(::std::error::Error + 'static),
        )?;
        let (implementation, _, clause) = generics.split_for_impl();
        output.extend(quote! {
            impl #implementation ::std::error::Error for #name #ty_generics #clause {
                fn source(&self) -> Option<&(dyn ::std::error::Error + 'static)> { match self { #(#sources),* } }
            }
        });
    }
    if classify {
        let generics = bounded(
            &input.generics,
            options.classify_bound.as_ref(),
            parse_quote!(ClassifyOperatorFailure),
        )?;
        let (implementation, _, clause) = generics.split_for_impl();
        output.extend(quote! {
            impl #implementation ClassifyOperatorFailure for #name #ty_generics #clause {
                fn operator_failure_class(&self) -> OperatorFailureClass { match self { #(#classes),* } }
                fn operator_failure_cause_code(&self) -> &'static str { match self { #(#codes),* } }
            }
        });
    }
    Ok(output)
}
