#![deny(deprecated)]

use mdbase::Collection;
use serde_json::Value;

pub fn released_04_facade_is_source_compatible(collection: &Collection, input: &Value) {
    let _ = collection.create(input);
    let _ = collection.update(input);
    let _ = collection.delete(input);
    let _ = collection.rename(input);
    let _ = collection.backfill(input);
    let _ = collection.batch_update(input, None, false);
    let _ = collection.batch_delete(input, None);
}
