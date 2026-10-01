//! The ADO subset behind `Server.CreateObject` (Milestone 6):
//! `ADODB.Connection`, `ADODB.Command`, and `ADODB.Recordset` plus
//! their `Fields`/`Field`/`Parameters`/`Parameter` collections.
//!
//! The engine stays database-blind: these states describe what ADO
//! calls were made, and every real database call goes through the
//! [`AdoHost`] trait, implemented by the runtime on the `asp-db`
//! adapters.
//!
//! Model notes shared with the plan doc:
//! - A recordset is a materialised snapshot; `MoveNext` etc. walk a
//!   cursor over it (`position` in `-1..=len`, `-1` = BOF, `len` =
//!   EOF, like ADO's ends).
//! - `Fields`/`Field`/`Parameters` objects are LIVE views: they hold
//!   the shared handle of their owner and read through it, so values
//!   change as the cursor moves (ADO behaviour).
//! - Connection strings are interpreted by the host (engine choice);
//!   unsupported ones error at `Open` with a clear message.

use crate::parser::Variant;
use asp_core::db::{AdoResult, AdoValue, DatabaseEngine};
use std::cell::RefCell;
use std::rc::Rc;

/// Database services a page render offers ADO objects.
pub trait AdoHost {
    /// Open a connection for a connection string; the shared engine
    /// handle goes into the connection object's state. An unparseable
    /// or failing connection string errors here (an open failure
    /// aborts the page, like a failing `Server.CreateObject`).
    /// Implementations own their state (interior mutability), matching
    /// the `NativeHost` `&self` discipline.
    fn db_open(&self, connection_string: &str) -> Result<DbEngineHandle, String>;
}

/// The ADO ProgIDs RASP can create in Milestone 6.
pub const ADO_REGISTRY: [&str; 6] = [
    "ADODB.Connection",
    "ADODB.Command",
    "ADODB.Recordset",
    "ADODB.Fields",
    "ADODB.Field",
    "ADODB.Parameter",
];

/// State of one live ADO object.
pub enum AdoState {
    Connection {
        /// `None` = not yet open (`Open` sets the shared engine).
        engine: Option<DbEngineHandle>,
        connection_string: String,
        /// `RecordsAffected` after the last `Connection.Execute`
        /// (-1 before the first execute; ADO exposes the same field
        /// through its out-parameter form).
        records_affected: i64,
        /// Live command objects bound to this connection; they
        /// activate with it (`ActiveConnection` semantics).
        commands: Vec<SharedAdo>,
    },
    Command {
        /// The connection this command runs on (`ActiveConnection`);
        /// its engine is dereferenced at execute time.
        connection: Option<SharedAdo>,
        sql: Option<String>,
        /// Live `ADODB.Parameter` objects appended via
        /// `Parameters.Append`; dereferenced at execute time so later
        /// `Value` writes apply (ADO behaviour).
        param_objs: Vec<SharedAdo>,
    },
    Recordset {
        result: AdoResult,
        /// Cursor: -1 = BOF, `len` = EOF, else a valid row index.
        position: i64,
        /// Set once the recordset has been opened/assigned; reads of
        /// an unopened recordset error.
        opened: bool,
    },
    Fields {
        /// The owning recordset (live view over its cursor).
        of: SharedAdo,
    },
    Field {
        /// The owning recordset plus this field's column ordinal.
        of: SharedAdo,
        index: usize,
    },
    Parameters {
        /// The owning command (live view over its parameter list).
        of: SharedAdo,
    },
    Parameter {
        name: String,
        /// The parameter's current value (writes apply to later
        /// executes, like ADO).
        value: Variant,
    },
}

impl AdoState {
    /// The full ProgID this state models.
    pub fn progid(&self) -> &'static str {
        match self {
            AdoState::Connection { .. } => "ADODB.Connection",
            AdoState::Command { .. } => "ADODB.Command",
            AdoState::Recordset { .. } => "ADODB.Recordset",
            AdoState::Fields { .. } => "ADODB.Fields",
            AdoState::Field { .. } => "ADODB.Field",
            AdoState::Parameters { .. } => "ADODB.Parameters",
            AdoState::Parameter { .. } => "ADODB.Parameter",
        }
    }

    /// The short display name (`Connection`, not `ADODB.Connection`),
    /// for output parity with the M5 objects.
    pub fn display_name(&self) -> &'static str {
        match self {
            AdoState::Connection { .. } => "Connection",
            AdoState::Command { .. } => "Command",
            AdoState::Recordset { .. } => "Recordset",
            AdoState::Fields { .. } => "Fields",
            AdoState::Field { .. } => "Field",
            AdoState::Parameters { .. } => "Parameters",
            AdoState::Parameter { .. } => "Parameter",
        }
    }
}

/// The shared cell every ADO object handle carries (`Native` variants
/// hold one of these; clone shares the same object like COM).
pub type SharedAdo = Rc<RefCell<AdoState>>;

/// A shared database engine handle inside a `Connection` state
/// (`None` before `Open`, dropped by `Close`).
pub type DbEngineHandle = Rc<RefCell<dyn DatabaseEngine>>;

/// Create the initial ADO object for a ProgID, or explain what is
/// available (error text matches the M5 registry style).
pub fn create_ado(progid: &str) -> Result<AdoState, String> {
    match progid {
        "ADODB.Connection" => Ok(AdoState::Connection {
            engine: None,
            connection_string: String::new(),
            records_affected: -1,
            commands: Vec::new(),
        }),
        "ADODB.Command" => Ok(AdoState::Command {
            connection: None,
            sql: None,
            param_objs: Vec::new(),
        }),
        "ADODB.Recordset" => Ok(AdoState::Recordset {
            result: AdoResult::empty(),
            position: -1,
            opened: false,
        }),
        "ADODB.Fields" => Ok(AdoState::Fields {
            of: Rc::new(RefCell::new(AdoState::Recordset {
                result: AdoResult::empty(),
                position: -1,
                opened: false,
            })),
        }),
        "ADODB.Field" => Ok(AdoState::Field {
            of: Rc::new(RefCell::new(AdoState::Recordset {
                result: AdoResult::empty(),
                position: -1,
                opened: false,
            })),
            index: 0,
        }),
        "ADODB.Parameter" => Ok(AdoState::Parameter {
            name: String::new(),
            value: Variant::Empty,
        }),
        other => Err(format!(
            "CreateObject: ProgID '{other}' is not available (ADO supports: {})",
            ADO_REGISTRY.join(", ")
        )),
    }
}

/// ADO row/param values convert to VBScript values. SQL `NULL` becomes
/// `Null` (rendering as `""`, like ADO's empty output for nulls).
pub fn ado_to_variant(v: &AdoValue) -> Variant {
    match v {
        AdoValue::Null => Variant::Null,
        AdoValue::Int(i) => Variant::Int(*i),
        AdoValue::Float(f) => Variant::Float(*f),
        AdoValue::Bool(b) => Variant::Bool(*b),
        AdoValue::Str(s) => Variant::Str(s.clone()),
    }
}

/// VBScript values convert to ADO row/param values (the reverse).
/// Objects and arrays cannot be stored; they error with a type
/// mismatch. Dates render through the invariant format (stored as
/// text; typed date storage arrives with the variant milestone).
pub fn variant_into_ado(v: &Variant) -> Result<AdoValue, String> {
    match v {
        Variant::Empty | Variant::Null => Ok(AdoValue::Null),
        Variant::Int(i) => Ok(AdoValue::Int(*i)),
        Variant::Float(f) => Ok(AdoValue::Float(*f)),
        Variant::Bool(b) => Ok(AdoValue::Bool(*b)),
        Variant::Str(s) => Ok(AdoValue::Str(s.clone())),
        Variant::Date(d) => Ok(AdoValue::Str(crate::vb_datetime::render(*d))),
        Variant::ObjectRef(_) | Variant::Native(_) | Variant::Arr(_) => Err(format!(
            "type mismatch: {} cannot be used as a database parameter",
            match v {
                Variant::ObjectRef(_) => "an object reference",
                Variant::Native(_) => "an object",
                _ => "an array",
            }
        )),
    }
}

/// ADO data-type constants accepted by `CreateParameter`'s type
/// argument. The value's storage type still derives from the assigned
/// variant (SQLite is dynamically typed; Postgres types derive from
/// the value); the argument is validated against the well-known
/// ranges rather than silently ignored.
pub fn is_known_ado_type(t: i64) -> bool {
    matches!(
        t,
        2..=6      // adSmallInt..adDouble
            | 7 | 8 | 10 | 11  // adDate, adBSTR, adError, adBoolean
            | 16..=21          // adTinyInt..adBigInt
            | 72 | 129 | 130 | 131 | 133 | 134 | 135 // guid, char/wchar, numeric, dates
            | 200..=205        // varchar/long/binary family
    )
}
