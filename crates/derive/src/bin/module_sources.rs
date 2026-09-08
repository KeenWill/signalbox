//! Lists the files reached by a Cargo target's Rust module declarations.

use std::collections::BTreeSet;
use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;

use syn::ext::IdentExt;
use syn::visit::{self, Visit};

struct Source {
    path: PathBuf,
    module_directory: PathBuf,
}

struct Modules<'a> {
    module_directory: PathBuf,
    path_directory: PathBuf,
    pending: &'a mut Vec<Source>,
    error: Option<syn::Error>,
}

fn explicit_path(module: &syn::ItemMod) -> syn::Result<Option<PathBuf>> {
    for attribute in &module.attrs {
        if attribute.path().is_ident("path") {
            let value = &attribute.meta.require_name_value()?.value;
            if let syn::Expr::Lit(expression) = value
                && let syn::Lit::Str(literal) = &expression.lit
            {
                return Ok(Some(PathBuf::from(literal.value())));
            }
            return Err(syn::Error::new_spanned(
                value,
                "module path must be a string",
            ));
        }
    }
    Ok(None)
}

impl<'ast> Visit<'ast> for Modules<'_> {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let explicit = match explicit_path(module) {
            Ok(path) => path,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let name = module.ident.unraw().to_string();
        if module.content.is_some() {
            let directory = explicit.map_or_else(
                || self.module_directory.join(&name),
                |path| self.path_directory.join(path),
            );
            let previous_module = std::mem::replace(&mut self.module_directory, directory.clone());
            let previous_path = std::mem::replace(&mut self.path_directory, directory);
            visit::visit_item_mod(self, module);
            self.module_directory = previous_module;
            self.path_directory = previous_path;
        } else {
            let candidates = explicit.as_ref().map_or_else(
                || {
                    vec![
                        self.module_directory.join(format!("{name}.rs")),
                        self.module_directory.join(name).join("mod.rs"),
                    ]
                },
                |path| vec![self.path_directory.join(path)],
            );
            for path in candidates {
                let module_directory = if explicit.is_some()
                    || path.file_name().is_some_and(|name| name == "mod.rs")
                {
                    path.with_file_name("")
                } else {
                    path.with_extension("")
                };
                self.pending.push(Source {
                    path,
                    module_directory,
                });
            }
        }
    }
}

fn module_sources(root: PathBuf) -> Result<BTreeSet<PathBuf>, Box<dyn Error>> {
    let mut pending = vec![Source {
        module_directory: root.parent().ok_or("target root has no parent")?.into(),
        path: root,
    }];
    let mut sources = BTreeSet::new();
    while let Some(source) = pending.pop() {
        if !source.path.is_file() {
            continue;
        }
        let path = source.path.canonicalize()?;
        if !sources.insert(path.clone()) {
            continue;
        }
        let syntax = syn::parse_file(&std::fs::read_to_string(&path)?)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut modules = Modules {
            module_directory: source.module_directory,
            path_directory: source.path.parent().ok_or("module has no parent")?.into(),
            pending: &mut pending,
            error: None,
        };
        modules.visit_file(&syntax);
        if let Some(error) = modules.error {
            return Err(error.into());
        }
    }
    Ok(sources)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut roots = std::env::args_os().skip(1).map(PathBuf::from);
    let mut sources = module_sources(roots.next().ok_or("missing library target root")?)?;
    for root in roots {
        for source in module_sources(root)? {
            sources.remove(&source);
        }
    }
    let mut output = io::stdout().lock();
    for source in sources {
        output.write_all(
            source
                .to_str()
                .ok_or("module path is not UTF-8")?
                .as_bytes(),
        )?;
        output.write_all(b"\0")?;
    }
    Ok(())
}
