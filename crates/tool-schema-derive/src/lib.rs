//! Derive support for `signalbox_tool_contract::ToolSchema`.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Attribute, Data, DeriveInput, Field, Fields, LitStr, Token, Type, parse_macro_input};

/// Derives an owned JSON Schema from one serde-decoded named-field struct.
///
/// Every admitted field requires `#[tool_schema(description = "...")]`.
/// Property names follow serde's `rename` and `rename_all` declarations. A
/// checked `name = "..."` may repeat the effective serde name, but cannot
/// contradict it. Fields whose type resolves to `Option<T>`, fields carrying `#[serde(default)]`,
/// and fields in a struct carrying `#[serde(default)]` are omitted from
/// `required`; a custom-decoded `Option<T>` remains required without a default.
/// `skip` and `skip_deserializing` omit a property. A `with = Type` override
/// declares the schema-bearing type for a custom serde decoder.
#[proc_macro_derive(ToolSchema, attributes(tool_schema, serde))]
pub fn derive_tool_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "ToolSchema derive does not yet support generic structs",
        ));
    }

    let container = parse_container_attributes(&input.attrs)?;
    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            Fields::Unnamed(_) | Fields::Unit => {
                return Err(syn::Error::new_spanned(
                    &input.ident,
                    "ToolSchema can only be derived for structs with named fields",
                ));
            }
        },
        Data::Enum(_) | Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "ToolSchema can only be derived for structs with named fields",
            ));
        }
    };

    let mut properties = Vec::new();
    let mut required = Vec::new();
    for field in fields {
        let Some(derived) = derive_field(field, container.rename_all.as_ref(), container.default)?
        else {
            continue;
        };
        properties.push(derived.property);
        required.push(derived.required_name);
    }

    let ident = &input.ident;
    let schema_name = ident.to_string();
    let deny_unknown_fields = container.deny_unknown_fields;
    Ok(quote! {
        impl ::signalbox_tool_contract::ToolSchema for #ident {
            fn schema() -> ::signalbox_tool_contract::__private::serde_json::Value {
                ::signalbox_tool_contract::__private::named_schema::<Self, _>(|| {
                    ::signalbox_tool_contract::__private::object_schema(
                        ::std::vec![#(#properties),*],
                        ::std::vec![#(#required),*],
                        #deny_unknown_fields,
                    )
                })
            }
        }

        impl ::signalbox_tool_contract::__private::schemars::JsonSchema for #ident {
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(#schema_name)
            }

            fn json_schema(
                generator: &mut ::signalbox_tool_contract::__private::schemars::SchemaGenerator,
            ) -> ::signalbox_tool_contract::__private::schemars::Schema {
                ::signalbox_tool_contract::__private::into_schemars_schema(
                    <Self as ::signalbox_tool_contract::ToolSchema>::schema(),
                    generator,
                )
            }

            fn inline_schema() -> bool {
                true
            }
        }
    })
}

#[derive(Default)]
struct ContainerAttributes {
    default: bool,
    deny_unknown_fields: bool,
    rename_all: Option<LitStr>,
}

fn parse_container_attributes(attributes: &[Attribute]) -> syn::Result<ContainerAttributes> {
    let mut parsed = ContainerAttributes::default();
    for attribute in attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("serde"))
    {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("deny_unknown_fields") {
                parsed.deny_unknown_fields = true;
                return Ok(());
            }
            if meta.path.is_ident("rename_all") {
                parsed.rename_all = parse_serde_name_value(&meta, "rename_all")?;
                return Ok(());
            }
            if meta.path.is_ident("default") {
                if meta.input.peek(Token![=]) {
                    let _path: LitStr = meta.value()?.parse()?;
                }
                parsed.default = true;
                return Ok(());
            }
            Err(meta.error(format!(
                "ToolSchema does not support this container serde attribute: `{}`",
                path_name(&meta.path)
            )))
        })?;
    }
    if let Some(rule) = parsed.rename_all.as_ref() {
        validate_rename_rule(&rule.value(), rule.span())?;
    }
    Ok(parsed)
}

struct DerivedField {
    property: TokenStream2,
    required_name: TokenStream2,
}

fn derive_field(
    field: &Field,
    rename_all: Option<&LitStr>,
    container_default: bool,
) -> syn::Result<Option<DerivedField>> {
    let ident = field
        .ident
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(field, "ToolSchema fields must have identifiers"))?;
    let rust_name = ident.to_string();
    let rust_name = rust_name.strip_prefix("r#").unwrap_or(&rust_name);
    let serde = parse_field_serde_attributes(field, rust_name)?;
    let schema = parse_field_schema_attributes(field, rust_name)?;

    if serde.skip_deserializing {
        if schema.description.is_some() || schema.name.is_some() || schema.with.is_some() {
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "field `{rust_name}`: tool_schema attributes contradict serde skip_deserializing"
                ),
            ));
        }
        return Ok(None);
    }
    if serde.flatten {
        return Err(syn::Error::new_spanned(
            field,
            format!(
                "field `{rust_name}`: serde flatten is not supported by ToolSchema; declare the fields explicitly"
            ),
        ));
    }
    if serde.custom_decoder && schema.with.is_none() {
        return Err(syn::Error::new_spanned(
            field,
            format!(
                "field `{rust_name}`: serde with/deserialize_with requires tool_schema(with = Type) to declare its wire shape"
            ),
        ));
    }
    if !serde.custom_decoder && schema.with.is_some() {
        return Err(syn::Error::new_spanned(
            field,
            format!(
                "field `{rust_name}`: tool_schema(with = Type) requires serde with/deserialize_with to define a custom wire shape"
            ),
        ));
    }

    ensure_supported_type(&field.ty, rust_name)?;
    let description = schema.description.ok_or_else(|| {
        syn::Error::new_spanned(
            field,
            format!("field `{rust_name}`: missing #[tool_schema(description = \"...\")]"),
        )
    })?;
    if description.value().is_empty() {
        return Err(syn::Error::new_spanned(
            description,
            format!("field `{rust_name}`: tool_schema description must not be empty"),
        ));
    }

    let effective_name = match serde.rename {
        Some(rename) => rename.value(),
        None => apply_rename_rule(rust_name, rename_all)?,
    };
    if let Some(declared_name) = schema.name
        && declared_name.value() != effective_name
    {
        return Err(syn::Error::new_spanned(
            declared_name,
            format!(
                "field `{rust_name}`: tool_schema name contradicts serde name `{effective_name}`; put the rename on #[serde(rename = \"...\")]"
            ),
        ));
    }

    let property_name = LitStr::new(&effective_name, ident.span());
    let schema_type = schema.with.as_ref().unwrap_or(&field.ty);
    let field_schema = format_ident!(
        "__signalbox_schema_for_field_{}",
        rust_name,
        span = ident.span()
    );
    let field_span = field.ty.span();
    let property = quote_spanned! {field_span=>
        (
            #property_name,
            {
                #[allow(non_snake_case)]
                fn #field_schema<Schema: ::signalbox_tool_contract::ToolSchema>()
                    -> ::signalbox_tool_contract::__private::serde_json::Value
                {
                    Schema::schema()
                }
                ::signalbox_tool_contract::__private::described_schema(
                    #field_schema::<#schema_type>(),
                    #description,
                )
            },
        )
    };
    let required_name = if container_default || serde.default {
        quote!(::std::option::Option::None)
    } else if serde.custom_decoder {
        quote!(::std::option::Option::Some(#property_name))
    } else {
        let field_type = &field.ty;
        quote_spanned! {field_span=>
            (!<#field_type as ::signalbox_tool_contract::ToolSchema>::is_optional())
                .then_some(#property_name)
        }
    };
    Ok(Some(DerivedField {
        property,
        required_name,
    }))
}

#[derive(Default)]
struct FieldSerdeAttributes {
    custom_decoder: bool,
    default: bool,
    flatten: bool,
    rename: Option<LitStr>,
    skip_deserializing: bool,
}

fn parse_field_serde_attributes(
    field: &Field,
    field_name: &str,
) -> syn::Result<FieldSerdeAttributes> {
    let mut parsed = FieldSerdeAttributes::default();
    for attribute in field
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("serde"))
    {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("default") {
                if meta.input.peek(Token![=]) {
                    let _path: LitStr = meta.value()?.parse()?;
                }
                parsed.default = true;
                return Ok(());
            }
            if meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing") {
                parsed.skip_deserializing = true;
                return Ok(());
            }
            if meta.path.is_ident("skip_serializing") {
                return Ok(());
            }
            if meta.path.is_ident("skip_serializing_if") {
                let _path: LitStr = meta.value()?.parse()?;
                return Ok(());
            }
            if meta.path.is_ident("flatten") {
                parsed.flatten = true;
                return Ok(());
            }
            if meta.path.is_ident("rename") {
                parsed.rename = parse_serde_name_value(&meta, "rename")?;
                return Ok(());
            }
            if meta.path.is_ident("with") || meta.path.is_ident("deserialize_with") {
                let _path: LitStr = meta.value()?.parse()?;
                parsed.custom_decoder = true;
                return Ok(());
            }
            if meta.path.is_ident("alias") {
                return Err(meta.error(format!(
                    "field `{field_name}`: serde aliases are not supported because one property schema cannot require either name"
                )));
            }
            if meta.path.is_ident("borrow") {
                if meta.input.peek(Token![=]) {
                    let _lifetimes: LitStr = meta.value()?.parse()?;
                }
                return Ok(());
            }
            Err(meta.error(format!(
                "field `{field_name}`: ToolSchema does not support this serde attribute: `{}`",
                path_name(&meta.path)
            )))
        })?;
    }
    Ok(parsed)
}

#[derive(Default)]
struct FieldSchemaAttributes {
    description: Option<LitStr>,
    name: Option<LitStr>,
    with: Option<Type>,
}

fn parse_field_schema_attributes(
    field: &Field,
    field_name: &str,
) -> syn::Result<FieldSchemaAttributes> {
    let mut parsed = FieldSchemaAttributes::default();
    for attribute in field
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("tool_schema"))
    {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("description") {
                let span = meta.path.span();
                set_once(
                    &mut parsed.description,
                    meta.value()?.parse()?,
                    span,
                    field_name,
                    "description",
                )?;
                return Ok(());
            }
            if meta.path.is_ident("name") {
                let span = meta.path.span();
                set_once(
                    &mut parsed.name,
                    meta.value()?.parse()?,
                    span,
                    field_name,
                    "name",
                )?;
                return Ok(());
            }
            if meta.path.is_ident("with") {
                let span = meta.path.span();
                set_once(
                    &mut parsed.with,
                    meta.value()?.parse()?,
                    span,
                    field_name,
                    "with",
                )?;
                return Ok(());
            }
            Err(meta.error(format!(
                "field `{field_name}`: unsupported tool_schema attribute; expected description, name, or with"
            )))
        })?;
    }
    Ok(parsed)
}

fn set_once<Value>(
    target: &mut Option<Value>,
    value: Value,
    span: Span,
    field_name: &str,
    attribute_name: &str,
) -> syn::Result<()> {
    if target.is_some() {
        return Err(syn::Error::new(
            span,
            format!("field `{field_name}`: duplicate tool_schema {attribute_name}"),
        ));
    }
    *target = Some(value);
    Ok(())
}

fn parse_serde_name_value(
    meta: &syn::meta::ParseNestedMeta<'_>,
    attribute_name: &str,
) -> syn::Result<Option<LitStr>> {
    if meta.input.peek(Token![=]) {
        return Ok(Some(meta.value()?.parse::<LitStr>()?));
    }
    let mut deserialize = None;
    meta.parse_nested_meta(|direction| {
        if direction.path.is_ident("deserialize") {
            deserialize = Some(direction.value()?.parse::<LitStr>()?);
            return Ok(());
        }
        if direction.path.is_ident("serialize") {
            let _serialize: LitStr = direction.value()?.parse()?;
            return Ok(());
        }
        Err(direction.error(format!("unsupported serde {attribute_name} direction")))
    })?;
    Ok(deserialize)
}

fn ensure_supported_type(ty: &Type, field_name: &str) -> syn::Result<()> {
    match ty {
        Type::Path(path) if path.qself.is_none() => Ok(()),
        _ => Err(syn::Error::new_spanned(
            ty,
            format!(
                "field `{field_name}`: unsupported schema type; use an owned path type implementing ToolSchema"
            ),
        )),
    }
}

fn apply_rename_rule(field_name: &str, rule: Option<&LitStr>) -> syn::Result<String> {
    let Some(rule) = rule else {
        return Ok(String::from(field_name));
    };
    let span = rule.span();
    let rule = rule.value();
    validate_rename_rule(&rule, span)?;
    Ok(match rule.as_str() {
        "lowercase" => String::from(field_name),
        "UPPERCASE" => field_name.to_ascii_uppercase(),
        "PascalCase" => pascal_case(field_name),
        "camelCase" => lower_first(&pascal_case(field_name)),
        "snake_case" => String::from(field_name),
        "SCREAMING_SNAKE_CASE" => field_name.to_ascii_uppercase(),
        "kebab-case" => field_name.replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => field_name.replace('_', "-").to_ascii_uppercase(),
        _ => String::from(field_name),
    })
}

fn validate_rename_rule(rule: &str, span: Span) -> syn::Result<()> {
    match rule {
        "lowercase"
        | "UPPERCASE"
        | "PascalCase"
        | "camelCase"
        | "snake_case"
        | "SCREAMING_SNAKE_CASE"
        | "kebab-case"
        | "SCREAMING-KEBAB-CASE" => Ok(()),
        _ => Err(syn::Error::new(
            span,
            format!("unsupported serde rename_all rule `{rule}`"),
        )),
    }
}

fn pascal_case(value: &str) -> String {
    value
        .split('_')
        .map(|word| {
            let mut characters = word.chars();
            match characters.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + characters.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn lower_first(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + characters.as_str(),
        None => String::new(),
    }
}

fn path_name(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}
