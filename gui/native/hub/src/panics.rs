//! Turning a caught panic into the text `HubFailed` shows.

use std::any::Any;

/// What `panic!` was given, when it was text, as it nearly always is.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic without a message".to_owned()
    }
}
