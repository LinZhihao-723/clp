//! JSON marshalling of table batches through compiled templates: one template per (table layout,
//! projection, options), made of pre-escaped static fragments and stateless value writers, rendered
//! into a reusable buffer of records with Arrow `Utf8` offsets.

mod buffer;
mod context;
mod error;
mod escape;
mod float;
mod logtype;
mod options;
mod template;
mod writer;

pub use buffer::JsonRecordBuffer;
pub use context::RenderContext;
pub use error::JsonError;
pub use options::JsonOptions;
pub use template::BoundJsonTemplate;
pub use template::JsonTemplate;

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}

    assert_send_sync::<RenderContext>();
    assert_send_sync::<JsonTemplate>();
    assert_send_sync::<JsonRecordBuffer>();
};
