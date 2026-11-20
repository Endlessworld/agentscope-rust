//! The backend a host opens without assembling a store itself.

use std::path::Path;
use std::sync::Arc;

use super::TxnBackend;
use crate::session_backend::SessionBackend;
use crate::store::file::FileStore;
use crate::store::RecordStore;
use crate::Result;

/// Opens a [`SessionBackend`] over one local store root.
///
/// This is the composition a host would otherwise repeat: [`FileStore`] for persistence and
/// [`TxnBackend`] for the state machine. A host that needs the store handle itself keeps using
/// [`TxnBackend::with_store`]; the two are equivalent.
pub struct LocalBackend;

impl LocalBackend {
    pub fn open(root: impl AsRef<Path>) -> Result<Arc<dyn SessionBackend>> {
        let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(root)?);
        Ok(Arc::new(TxnBackend::with_store(store)))
    }

    pub fn in_memory() -> Arc<dyn SessionBackend> {
        Arc::new(TxnBackend::in_memory())
    }
}
