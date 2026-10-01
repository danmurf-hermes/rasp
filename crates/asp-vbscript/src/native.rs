//! Native objects behind `Server.CreateObject` (M5): a ProgID registry
//! plus the two built-in objects, `Scripting.FileSystemObject` and
//! `Scripting.Dictionary`.
//!
//! File operations are NOT performed here: the engine stays
//! filesystem-blind and calls the `NativeHost` trait, which the
//! runtime implements with app-root confinement.

use crate::parser::Variant;
use std::cell::RefCell;
use std::rc::Rc;

/// The two ProgIDs RASP could create before M6 (kept for the registry
/// error message; ADO ProgIDs are appended at runtime).
pub const REGISTRY: [&str; 2] = ["Scripting.FileSystemObject", "Scripting.Dictionary"];

/// State of one live native object.
pub enum NativeState {
    /// `Scripting.FileSystemObject`: operations all go through the host.
    FileSystem,
    /// `Scripting.Dictionary`: insertion-ordered key/value pairs with
    /// case-sensitive Variant keys (VBScript binary-compare mode).
    Dictionary(Vec<(Variant, Variant)>),
    /// An ADO object (`ADODB.*`, M6): state lives in the shared cell.
    Ado(super::ado::SharedAdo),
}

impl NativeState {
    /// The ProgID this state came from.
    pub fn progid(&self) -> &'static str {
        match self {
            NativeState::FileSystem => "Scripting.FileSystemObject",
            NativeState::Dictionary(_) => "Scripting.Dictionary",
            NativeState::Ado(cell) => {
                let state = cell.borrow();
                state.progid()
            }
        }
    }
}

/// The full ProgID list RASP can create (M5 objects + ADO, M6).
pub fn registry_all() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = REGISTRY.to_vec();
    all.extend(super::ado::ADO_REGISTRY);
    all
}

/// One live native object. `Clone` shares the same object (reference
/// semantics, like COM); identity comparison uses `Rc::ptr_eq`-style
/// pointer equality via [`Variant::Native`].
pub struct NativeObj {
    pub state: RefCell<NativeState>,
}

impl NativeObj {
    pub fn new(state: NativeState) -> Self {
        Self {
            state: RefCell::new(state),
        }
    }

    /// A shared handle for this object.
    pub fn share(self) -> Rc<NativeObj> {
        Rc::new(self)
    }
}

impl std::fmt::Debug for NativeObj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.borrow();
        match &*state {
            NativeState::FileSystem => write!(f, "NativeObj(FileSystemObject)"),
            NativeState::Dictionary(pairs) => {
                write!(f, "NativeObj(Dictionary, {} items)", pairs.len())
            }
            NativeState::Ado(cell) => {
                let ado = cell.borrow();
                write!(f, "NativeObj({})", ado.display_name())
            }
        }
    }
}

/// Create a native object for a ProgID (`Scripting.*` locally, `ADODB.*`
/// through the ADO factory), or explain what is available.
pub fn create_native(progid: &str) -> Result<Rc<NativeObj>, String> {
    match progid {
        "Scripting.FileSystemObject" => Ok(NativeObj::new(NativeState::FileSystem).share()),
        "Scripting.Dictionary" => Ok(NativeObj::new(NativeState::Dictionary(Vec::new())).share()),
        ado if super::ado::ADO_REGISTRY.contains(&ado) => {
            let state = super::ado::create_ado(ado)?;
            Ok(NativeObj::new(NativeState::Ado(Rc::new(RefCell::new(state)))).share())
        }
        other => Err(format!(
            "CreateObject: ProgID '{other}' is not available (supported: {})",
            registry_all().join(", ")
        )),
    }
}

/// Host services a page render can offer native objects. Paths are
/// app-root-relative POSIX paths; the host enforces confinement and
/// rejects escape attempts with a clear error.
pub trait NativeHost {
    /// `Server.MapPath`: normalise a virtual/root-relative path,
    /// rejecting traversal outside the root.
    fn map_path(&self, path: &str) -> Result<String, String>;
    fn file_exists(&self, path: &str) -> Result<bool, String>;
    fn folder_exists(&self, path: &str) -> Result<bool, String>;
    fn read_text_file(&self, path: &str) -> Result<String, String>;
    fn create_text_file(&self, path: &str, content: &str, overwrite: bool) -> Result<(), String>;
    fn append_text_file(&self, path: &str, content: &str) -> Result<(), String>;
    fn delete_file(&self, path: &str, force: bool) -> Result<(), String>;
    fn delete_folder(&self, path: &str) -> Result<(), String>;
    fn create_folder(&self, path: &str) -> Result<(), String>;
    fn copy_file(&self, from: &str, to: &str, overwrite: bool) -> Result<(), String>;
    fn move_file(&self, from: &str, to: &str) -> Result<(), String>;
    fn list_folder(&self, path: &str) -> Result<Vec<String>, String>;
    /// Render another page of this application inline
    /// (`Server.Execute`/`Server.Transfer`). Shares nothing else; the
    /// caller merges the returned body/cookies into its own response.
    fn render_sub_page(&self, path: &str) -> Result<SubPage, String>;
}

/// What `Server.Execute`/`Transfer` get back from the host.
#[derive(Debug, Clone, PartialEq)]
pub struct SubPage {
    pub body: String,
    pub status: u16,
    pub set_cookies: Vec<String>,
}
