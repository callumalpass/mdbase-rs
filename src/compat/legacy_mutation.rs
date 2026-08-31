//! Source-compatible 0.4.x ambient mutation facade.
//!
//! This module owns the only public context-free mutation entry points. It is
//! selected by the default-on `legacy-collection-mutation` feature. Rust
//! deprecation attributes intentionally begin only in 0.5.0 so 0.4.x consumers
//! using `deny(deprecated)` retain strict source compatibility.

use crate::Collection;

impl Collection {
    /// **Deprecated compatibility API — planned removal: 0.5.0.**
    ///
    /// Use `Collection::typed()?.create(CreateRequest)`.
    pub fn create(&self, input: &serde_json::Value) -> serde_json::Value {
        self.create_legacy(input)
    }

    /// **Deprecated compatibility API — planned removal: 0.5.0.**
    ///
    /// Use `Collection::typed()?.update(UpdateRequest)`.
    pub fn update(&self, input: &serde_json::Value) -> serde_json::Value {
        self.update_legacy(input)
    }

    /// **Deprecated compatibility API — planned removal: 0.5.0.**
    ///
    /// Use `Collection::typed()?.delete(DeleteRequest)` or `preflight_delete`.
    pub fn delete(&self, input: &serde_json::Value) -> serde_json::Value {
        self.delete_legacy(input)
    }

    /// **Deprecated compatibility API — planned removal: 0.5.0.**
    ///
    /// Use `Collection::typed()?.rename(RenameRequest)` or `preflight_rename`.
    pub fn rename(&self, input: &serde_json::Value) -> serde_json::Value {
        self.rename_legacy(input)
    }

    /// **Deprecated compatibility API — planned removal: 0.5.0.**
    ///
    /// No canonical backfill operation exists. Use typed query/read followed by
    /// `Collection::typed()?.batch(BatchRequest)`.
    pub fn backfill(&self, input: &serde_json::Value) -> serde_json::Value {
        self.backfill_legacy(input)
    }

    /// **Deprecated compatibility API — planned removal: 0.5.0.**
    ///
    /// Use `Collection::typed()?.batch(BatchRequest)`.
    pub fn batch_update(
        &self,
        input: &serde_json::Value,
        simulate_io_error: Option<&str>,
        skip_dependents: bool,
    ) -> serde_json::Value {
        self.batch_update_legacy(input, simulate_io_error, skip_dependents)
    }

    /// **Deprecated compatibility API — planned removal: 0.5.0.**
    ///
    /// Use `Collection::typed()?.batch(BatchRequest)`.
    pub fn batch_delete(
        &self,
        input: &serde_json::Value,
        simulate_io_error: Option<&str>,
    ) -> serde_json::Value {
        self.batch_delete_legacy(input, simulate_io_error)
    }
}
