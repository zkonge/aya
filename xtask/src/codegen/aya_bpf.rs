use std::{fs::File, io::Write, path::PathBuf, process::Command};

use anyhow::anyhow;
use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use structopt::StructOpt;
use syn::{
    self, parse_str,
    punctuated::Punctuated,
    token::Comma,
    visit_mut::{self, VisitMut},
    AngleBracketedGenericArguments, ForeignItemStatic, GenericArgument, Ident, Item,
    PathArguments::AngleBracketed,
    Type,
};

use crate::codegen::{
    bindings::bindgen,
    getters::{generate_getters_for_items, Getter},
};

#[derive(StructOpt)]
pub struct CodegenOptions {
    #[structopt(long)]
    libbpf_dir: PathBuf,
}

pub fn codegen(opts: CodegenOptions) -> Result<(), anyhow::Error> {
    let dir = PathBuf::from("bpf/aya-bpf");
    let generated = dir.join("src/bpf/generated");

    let types = ["bpf_map_.*"];
    let vars = ["BPF_.*", "bpf_.*"];
    let mut cmd = bindgen(&types, &vars);
    cmd.arg(&*dir.join("include/aya_bpf_bindings.h").to_string_lossy());
    cmd.arg("--");
    cmd.arg("-I").arg(opts.libbpf_dir.join("src"));

    let output = cmd.output()?;
    let bindings = std::str::from_utf8(&output.stdout)?;

    if !output.status.success() {
        eprintln!("{}", std::str::from_utf8(&output.stderr)?);
        return Err(anyhow!("bindgen failed: {}", output.status));
    }

    // delete the helpers, then rewrite them in helpers.rs
    let mut tree = parse_str::<syn::File>(bindings).unwrap();

    let mut tx = RewriteBpfHelpers {
        helpers: Vec::new(),
    };
    tx.visit_file_mut(&mut tree);

    let bindings = tree.to_token_stream().to_string();
    let filename = generated.join("bindings.rs");
    {
        let mut file = File::create(&filename)?;
        write!(file, "{}", bindings)?;
    }
    Command::new("rustfmt").arg(filename).status()?;

    let filename = generated.join("helpers.rs");
    {
        let mut file = File::create(&filename)?;
        write!(file, "use crate::bpf::generated::bindings::*;")?;
        for helper in &tx.helpers {
            file.write(helper.as_bytes())?;
        }
    }
    Command::new("rustfmt").arg(filename).status()?;

    let getters = generate_getters_for_items(&tree.items, gen_probe_read_getter);
    let filename = generated.join("getters.rs");
    {
        let mut file = File::create(&filename)?;
        write!(file, "use crate::bpf::generated::bindings::*;")?;
        write!(file, "{}", getters)?;
    }
    Command::new("rustfmt").arg(filename).status()?;

    Ok(())
}

fn gen_probe_read_getter(getter: &Getter<'_>) -> TokenStream {
    let ident = getter.ident;
    let ty = getter.ty;
    let prefix = &getter.prefix;
    match ty {
        Type::Ptr(_) => {
            quote! {
                pub fn #ident(&self) -> Option<#ty> {
                    let v = unsafe { crate::bpf::helpers::bpf_probe_read(&#(#prefix).*.#ident) }.ok()?;
                    if v.is_null() {
                        None
                    } else {
                        Some(v)
                    }
                }
            }
        }
        _ => {
            quote! {
                pub fn #ident(&self) -> Option<#ty> {
                    unsafe { crate::bpf::helpers::bpf_probe_read(&#(#prefix).*.#ident) }.ok()
                }
            }
        }
    }
}

struct RewriteBpfHelpers {
    helpers: Vec<String>,
}

impl VisitMut for RewriteBpfHelpers {
    fn visit_item_mut(&mut self, item: &mut Item) {
        visit_mut::visit_item_mut(self, item);
        if let Item::ForeignMod(_) = item {
            *item = Item::Verbatim(TokenStream::new())
        }
    }
    fn visit_foreign_item_static_mut(&mut self, static_item: &mut ForeignItemStatic) {
        if let Type::Path(path) = &*static_item.ty {
            let ident = &static_item.ident;
            let ident_str = ident.to_string();
            let last = path.path.segments.last().unwrap();
            let ty_ident = last.ident.to_string();
            if ident_str.starts_with("bpf_") && ty_ident == "Option" {
                let fn_ty = match &last.arguments {
                    AngleBracketed(AngleBracketedGenericArguments { args, .. }) => {
                        args.first().unwrap()
                    }
                    _ => panic!(),
                };
                let mut ty_s = quote! {
                    #[inline(always)]
                    pub #fn_ty
                }
                .to_string();
                ty_s = ty_s.replace("fn (", &format!("fn {} (", ident_str));
                let call_idx = self.helpers.len() + 1;
                let args: Punctuated<Ident, Comma> = match fn_ty {
                    GenericArgument::Type(Type::BareFn(f)) => f
                        .inputs
                        .iter()
                        .map(|arg| arg.name.clone().unwrap().0)
                        .collect(),
                    _ => unreachable!(),
                };
                let body = quote! {
                    {
                        let f: #fn_ty = ::core::mem::transmute(#call_idx);
                        f(#args)
                    }
                }
                .to_string();
                ty_s.push_str(&body);
                let mut helper = ty_s;
                if helper.contains("printk") {
                    helper = format!("/* {} */", helper);
                }
                self.helpers.push(helper);
            }
        }
    }
}
