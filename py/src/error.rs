//! Mapping [`oxihipo::HipoError`] onto a Python exception tree.
//!
//! The orphan rule forbids `impl From<HipoError> for PyErr` here (both types
//! are foreign), so conversion goes through [`to_pyerr`]. Two custom
//! exceptions root the tree; the rest map onto builtins a physicist already
//! catches (`KeyError` for a missing bank/column, `TypeError` for a wrong
//! dtype, `OSError` for I/O).

use oxihipo::HipoError;
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyKeyError, PyOSError, PyTypeError, PyValueError};
use pyo3::prelude::*;

create_exception!(
    _oxihipo,
    OxihipoError,
    PyException,
    "Base class for every oxihipo error."
);
create_exception!(
    _oxihipo,
    CorruptFileError,
    OxihipoError,
    "A HIPO file or record was malformed, truncated, or failed to decompress."
);

/// Convert a core error into the matching Python exception. A `Path`-wrapped
/// error keeps the underlying variant's exception class but reports the full
/// message (which already includes the offending path).
pub(crate) fn to_pyerr(err: HipoError) -> PyErr {
    fn build(e: &HipoError, msg: String) -> PyErr {
        match e {
            HipoError::Io(_) => PyOSError::new_err(msg),
            HipoError::UnknownSchema { .. } | HipoError::UnknownColumn { .. } => {
                PyKeyError::new_err(msg)
            }
            HipoError::TypeMismatch { .. } | HipoError::ColumnLengthMismatch { .. } => {
                PyTypeError::new_err(msg)
            }
            HipoError::SchemaParse(_) | HipoError::InvalidGlob { .. } => PyValueError::new_err(msg),
            // Unwrap the path context to classify by the underlying cause.
            HipoError::Path { source, .. } => build(source, msg),
            // Corrupt/decompress/version/magic and anything added later
            // (the enum is #[non_exhaustive]) → the file-integrity leaf.
            _ => CorruptFileError::new_err(msg),
        }
    }
    let msg = err.to_string();
    build(&err, msg)
}

/// Register the custom exception types on the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("OxihipoError", m.py().get_type::<OxihipoError>())?;
    m.add("CorruptFileError", m.py().get_type::<CorruptFileError>())?;
    Ok(())
}
