//! Execute this fork's storage implementation, without building V8.
//! Only its runtime adapter (spawn_blocking and wall clock) is supplied here.

use object_store::{list::PaginatedListStore, ObjectStore};
use std::{path::Path, sync::Arc};

include!(concat!(env!("OUT_DIR"), "/local_store_module.rs"));

pub mod asyncrt {
    pub fn wall_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    pub fn blocking<T, F>(operation: F) -> tokio::task::JoinHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(operation)
    }
}

#[derive(Clone)]
pub struct Stores {
    pub objects: Arc<dyn ObjectStore>,
    pub listing: Arc<dyn PaginatedListStore>,
}

pub fn local(database: impl AsRef<Path>) -> anyhow::Result<Stores> {
    let store = Arc::new(local_store::LocalStore::open(database)?);
    Ok(Stores {
        objects: store.clone(),
        listing: store,
    })
}

pub fn azure(endpoint: &str, container: &str) -> anyhow::Result<Stores> {
    // object_store 0.12.5 reads the emulator URL from this variable; its
    // with_endpoint option only applies to non-emulator clients.
    std::env::set_var("AZURITE_BLOB_STORAGE_URL", endpoint);
    let store = Arc::new(
        object_store::azure::MicrosoftAzureBuilder::new()
            .with_use_emulator(true)
            .with_endpoint(endpoint.to_string())
            .with_container_name(container)
            .with_retry(object_store::RetryConfig {
                max_retries: 0,
                ..Default::default()
            })
            .build()?,
    );
    Ok(Stores {
        objects: store.clone(),
        listing: store,
    })
}
