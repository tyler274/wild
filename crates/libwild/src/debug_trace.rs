//! Sets up a tracing layer for debugging the linker.

use crate::error::AlreadyInitialised;

/// All trace messages within a span with this name will be emitted.
pub(crate) const TRACE_SPAN_NAME: &str = "trace_file";

pub(crate) fn span_for_file(
    args: &impl crate::platform::Args,
    file_id: crate::input_data::FileId,
) -> Option<tracing::span::EnteredSpan> {
    args.should_trace_file(file_id)
        .then(|| tracing::trace_span!(TRACE_SPAN_NAME).entered())
}

pub(crate) fn init() -> Result<(), AlreadyInitialised> {
    use tracing_subscriber::prelude::*;

    let filter = tracing_subscriber::filter::DynFilterFn::new(|metadata, cx| {
        if metadata.is_span() && metadata.name() == TRACE_SPAN_NAME {
            return true;
        }
        let mut current = cx.lookup_current();
        while let Some(span) = current {
            if span.name() == TRACE_SPAN_NAME {
                return true;
            }
            current = span.parent();
        }
        false
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter))
        .try_init()
        .map_err(|_| AlreadyInitialised)
}
