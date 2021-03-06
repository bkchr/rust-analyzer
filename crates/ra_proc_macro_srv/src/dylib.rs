//! Handles dynamic library loading for proc macro

use crate::{proc_macro::bridge, rustc_server::TokenStream};
use std::path::Path;

use goblin::{mach::Mach, Object};
use libloading::Library;
use ra_proc_macro::ProcMacroKind;

use std::io::Error as IoError;
use std::io::ErrorKind as IoErrorKind;

const NEW_REGISTRAR_SYMBOL: &str = "_rustc_proc_macro_decls_";

fn invalid_data_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> IoError {
    IoError::new(IoErrorKind::InvalidData, e)
}

fn is_derive_registrar_symbol(symbol: &str) -> bool {
    symbol.contains(NEW_REGISTRAR_SYMBOL)
}

fn find_registrar_symbol(file: &Path) -> Result<Option<String>, IoError> {
    let buffer = std::fs::read(file)?;
    let object = Object::parse(&buffer).map_err(invalid_data_err)?;

    match object {
        Object::Elf(elf) => {
            let symbols = elf.dynstrtab.to_vec().map_err(invalid_data_err)?;
            let name =
                symbols.iter().find(|s| is_derive_registrar_symbol(s)).map(|s| s.to_string());
            Ok(name)
        }
        Object::PE(pe) => {
            let name = pe
                .exports
                .iter()
                .flat_map(|s| s.name)
                .find(|s| is_derive_registrar_symbol(s))
                .map(|s| s.to_string());
            Ok(name)
        }
        Object::Mach(Mach::Binary(binary)) => {
            let exports = binary.exports().map_err(invalid_data_err)?;
            let name = exports
                .iter()
                .map(|s| {
                    // In macos doc:
                    // https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/dlsym.3.html
                    // Unlike other dyld API's, the symbol name passed to dlsym() must NOT be
                    // prepended with an underscore.
                    if s.name.starts_with("_") {
                        &s.name[1..]
                    } else {
                        &s.name
                    }
                })
                .find(|s| is_derive_registrar_symbol(&s))
                .map(|s| s.to_string());
            Ok(name)
        }
        _ => Ok(None),
    }
}

/// Loads dynamic library in platform dependent manner.
///
/// For unix, you have to use RTLD_DEEPBIND flag to escape problems described
/// [here](https://github.com/fedochet/rust-proc-macro-panic-inside-panic-expample)
/// and [here](https://github.com/rust-lang/rust/issues/60593).
///
/// Usage of RTLD_DEEPBIND
/// [here](https://github.com/fedochet/rust-proc-macro-panic-inside-panic-expample/issues/1)
///
/// It seems that on Windows that behaviour is default, so we do nothing in that case.
#[cfg(windows)]
fn load_library(file: &Path) -> Result<Library, libloading::Error> {
    Library::new(file)
}

#[cfg(unix)]
fn load_library(file: &Path) -> Result<Library, libloading::Error> {
    use libloading::os::unix::Library as UnixLibrary;
    use std::os::raw::c_int;

    const RTLD_NOW: c_int = 0x00002;
    const RTLD_DEEPBIND: c_int = 0x00008;

    UnixLibrary::open(Some(file), RTLD_NOW | RTLD_DEEPBIND).map(|lib| lib.into())
}

struct ProcMacroLibraryLibloading {
    // Hold the dylib to prevent it for unloadeding
    _lib: Library,
    exported_macros: Vec<bridge::client::ProcMacro>,
}

impl ProcMacroLibraryLibloading {
    fn open(file: &Path) -> Result<Self, IoError> {
        let symbol_name = find_registrar_symbol(file)?
            .ok_or(invalid_data_err(format!("Cannot find registrar symbol in file {:?}", file)))?;

        let lib = load_library(file).map_err(invalid_data_err)?;
        let exported_macros = {
            let macros: libloading::Symbol<&&[bridge::client::ProcMacro]> =
                unsafe { lib.get(symbol_name.as_bytes()) }.map_err(invalid_data_err)?;
            macros.to_vec()
        };

        Ok(ProcMacroLibraryLibloading { _lib: lib, exported_macros })
    }
}

type ProcMacroLibraryImpl = ProcMacroLibraryLibloading;

pub struct Expander {
    libs: Vec<ProcMacroLibraryImpl>,
}

impl Expander {
    pub fn new<P: AsRef<Path>>(lib: &P) -> Result<Expander, String> {
        let mut libs = vec![];
        /* Some libraries for dynamic loading require canonicalized path (even when it is
        already absolute
        */
        let lib =
            lib.as_ref().canonicalize().expect(&format!("Cannot canonicalize {:?}", lib.as_ref()));

        let library = ProcMacroLibraryImpl::open(&lib).map_err(|e| e.to_string())?;
        libs.push(library);

        Ok(Expander { libs })
    }

    pub fn expand(
        &self,
        macro_name: &str,
        macro_body: &ra_tt::Subtree,
        attributes: Option<&ra_tt::Subtree>,
    ) -> Result<ra_tt::Subtree, bridge::PanicMessage> {
        let parsed_body = TokenStream::with_subtree(macro_body.clone());

        let parsed_attributes = attributes
            .map_or(crate::rustc_server::TokenStream::new(), |attr| {
                TokenStream::with_subtree(attr.clone())
            });

        for lib in &self.libs {
            for proc_macro in &lib.exported_macros {
                match proc_macro {
                    bridge::client::ProcMacro::CustomDerive { trait_name, client, .. }
                        if *trait_name == macro_name =>
                    {
                        let res = client.run(
                            &crate::proc_macro::bridge::server::SameThread,
                            crate::rustc_server::Rustc::default(),
                            parsed_body,
                        );
                        return res.map(|it| it.subtree);
                    }
                    bridge::client::ProcMacro::Bang { name, client } if *name == macro_name => {
                        let res = client.run(
                            &crate::proc_macro::bridge::server::SameThread,
                            crate::rustc_server::Rustc::default(),
                            parsed_body,
                        );
                        return res.map(|it| it.subtree);
                    }
                    bridge::client::ProcMacro::Attr { name, client } if *name == macro_name => {
                        let res = client.run(
                            &crate::proc_macro::bridge::server::SameThread,
                            crate::rustc_server::Rustc::default(),
                            parsed_attributes,
                            parsed_body,
                        );

                        return res.map(|it| it.subtree);
                    }
                    _ => continue,
                }
            }
        }

        Err(bridge::PanicMessage::String("Nothing to expand".to_string()))
    }

    pub fn list_macros(&self) -> Result<Vec<(String, ProcMacroKind)>, bridge::PanicMessage> {
        let mut result = vec![];

        for lib in &self.libs {
            for proc_macro in &lib.exported_macros {
                let res = match proc_macro {
                    bridge::client::ProcMacro::CustomDerive { trait_name, .. } => {
                        (trait_name.to_string(), ProcMacroKind::CustomDerive)
                    }
                    bridge::client::ProcMacro::Bang { name, .. } => {
                        (name.to_string(), ProcMacroKind::FuncLike)
                    }
                    bridge::client::ProcMacro::Attr { name, .. } => {
                        (name.to_string(), ProcMacroKind::Attr)
                    }
                };
                result.push(res);
            }
        }

        Ok(result)
    }
}
