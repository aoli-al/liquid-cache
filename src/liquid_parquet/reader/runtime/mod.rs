use std::{
    collections::VecDeque,
    fmt::Formatter,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use arrow::{
    array::{Array, RecordBatch},
    compute::prep_null_mask_filter,
};
use arrow_schema::SchemaRef;
use futures::{future::BoxFuture, ready, FutureExt, Stream};
use in_memory_rg::InMemoryRowGroup;
use parquet::{
    arrow::{
        array_reader::ArrayReader,
        arrow_reader::{ArrowPredicate, ParquetRecordBatchReader, RowSelection, RowSelector},
        async_reader::{AsyncFileReader, ParquetRecordBatchStream},
        ProjectionMask,
    },
    errors::ParquetError,
    file::metadata::ParquetMetaData,
};
use parquet_bridge::{
    limit_row_selection, offset_row_selection, ParquetField, ParquetRecordBatchReaderInner,
};

mod in_memory_rg;
mod parquet_bridge;

pub struct LiquidRowFilter {
    pub(crate) predicates: Vec<Box<dyn ArrowPredicate>>,
}

type ReadResult = Result<(ReaderFactory, Option<ParquetRecordBatchReader>), ParquetError>;

struct ReaderFactory {
    metadata: Arc<ParquetMetaData>,

    fields: Option<Arc<ParquetField>>,

    input: Box<dyn AsyncFileReader>,

    filter: Option<LiquidRowFilter>,

    limit: Option<usize>,

    offset: Option<usize>,
}

impl ReaderFactory {
    /// Reads the next row group with the provided `selection`, `projection` and `batch_size`
    ///
    /// Note: this captures self so that the resulting future has a static lifetime
    async fn read_row_group(
        mut self,
        row_group_idx: usize,
        mut selection: Option<RowSelection>,
        projection: ProjectionMask,
        batch_size: usize,
    ) -> ReadResult {
        let meta = self.metadata.row_group(row_group_idx);
        let offset_index = self
            .metadata
            .offset_index()
            // filter out empty offset indexes (old versions specified Some(vec![]) when no present)
            .filter(|index| !index.is_empty())
            .map(|x| x[row_group_idx].as_slice());

        let mut row_group = InMemoryRowGroup {
            metadata: meta,
            // schema: meta.schema_descr_ptr(),
            row_count: meta.num_rows() as usize,
            column_chunks: vec![None; meta.columns().len()],
            offset_index,
        };

        if let Some(filter) = self.filter.as_mut() {
            for predicate in filter.predicates.iter_mut() {
                if !selects_any(selection.as_ref()) {
                    return Ok((self, None));
                }

                let predicate_projection = predicate.projection();
                row_group
                    .fetch(&mut self.input, predicate_projection, selection.as_ref())
                    .await?;

                let array_reader = parquet::arrow::array_reader::build_array_reader(
                    #[allow(clippy::missing_transmute_annotations)]
                    unsafe {
                        std::mem::transmute(self.fields.as_deref())
                    },
                    predicate_projection,
                    &row_group,
                )?;

                selection = Some(evaluate_predicate(
                    batch_size,
                    array_reader,
                    selection,
                    predicate.as_mut(),
                )?);
            }
        }

        // Compute the number of rows in the selection before applying limit and offset
        let rows_before = selection
            .as_ref()
            .map(|s| s.row_count())
            .unwrap_or(row_group.row_count);

        if rows_before == 0 {
            return Ok((self, None));
        }

        selection = apply_range(selection, row_group.row_count, self.offset, self.limit);

        // Compute the number of rows in the selection after applying limit and offset
        let rows_after = selection
            .as_ref()
            .map(|s| s.row_count())
            .unwrap_or(row_group.row_count);

        // Update offset if necessary
        if let Some(offset) = &mut self.offset {
            // Reduction is either because of offset or limit, as limit is applied
            // after offset has been "exhausted" can just use saturating sub here
            *offset = offset.saturating_sub(rows_before - rows_after)
        }

        if rows_after == 0 {
            return Ok((self, None));
        }

        if let Some(limit) = &mut self.limit {
            *limit -= rows_after;
        }

        row_group
            .fetch(&mut self.input, &projection, selection.as_ref())
            .await?;

        let reader = ParquetRecordBatchReaderInner::new_parquet(
            batch_size,
            parquet::arrow::array_reader::build_array_reader(
                #[allow(clippy::missing_transmute_annotations)]
                unsafe {
                    std::mem::transmute(self.fields.as_deref())
                },
                &projection,
                &row_group,
            )?,
            selection,
        );

        Ok((self, Some(reader)))
    }
}

pub(crate) fn evaluate_predicate(
    batch_size: usize,
    array_reader: Box<dyn ArrayReader>,
    input_selection: Option<RowSelection>,
    predicate: &mut dyn ArrowPredicate,
) -> Result<RowSelection, ParquetError> {
    let reader = ParquetRecordBatchReaderInner::new_parquet(
        batch_size,
        array_reader,
        input_selection.clone(),
    );
    let mut filters = vec![];
    for maybe_batch in reader {
        let maybe_batch = maybe_batch?;
        let input_rows = maybe_batch.num_rows();
        let filter = predicate.evaluate(maybe_batch)?;
        // Since user supplied predicate, check error here to catch bugs quickly
        if filter.len() != input_rows {
            return Err(ParquetError::ArrowError(
                format! {"ArrowPredicate predicate returned {} rows, expected {input_rows}",
                        filter.len(),
                },
            ));
        }
        match filter.null_count() {
            0 => filters.push(filter),
            _ => filters.push(prep_null_mask_filter(&filter)),
        };
    }

    let raw = RowSelection::from_filters(&filters);
    Ok(match input_selection {
        Some(selection) => selection.and_then(&raw),
        None => raw,
    })
}

/// Returns `true` if `selection` is `None` or selects some rows
pub(crate) fn selects_any(selection: Option<&RowSelection>) -> bool {
    selection.map(|x| x.selects_any()).unwrap_or(true)
}

/// Applies an optional offset and limit to an optional [`RowSelection`]
pub(crate) fn apply_range(
    mut selection: Option<RowSelection>,
    row_count: usize,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Option<RowSelection> {
    // If an offset is defined, apply it to the `selection`
    if let Some(offset) = offset {
        selection = Some(match row_count.checked_sub(offset) {
            None => RowSelection::from(vec![]),
            Some(remaining) => selection
                .map(|selection| offset_row_selection(selection, offset))
                .unwrap_or_else(|| {
                    RowSelection::from(vec![
                        RowSelector::skip(offset),
                        RowSelector::select(remaining),
                    ])
                }),
        });
    }

    // If a limit is defined, apply it to the final `selection`
    if let Some(limit) = limit {
        selection = Some(
            selection
                .map(|selection| limit_row_selection(selection, limit))
                .unwrap_or_else(|| {
                    RowSelection::from(vec![RowSelector::select(limit.min(row_count))])
                }),
        );
    }
    selection
}

enum StreamState {
    /// At the start of a new row group, or the end of the parquet stream
    Init,
    /// Decoding a batch
    Decoding(ParquetRecordBatchReader),
    /// Reading data from input
    Reading(BoxFuture<'static, ReadResult>),
    /// Error
    Error,
}

impl std::fmt::Debug for StreamState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamState::Init => write!(f, "StreamState::Init"),
            StreamState::Decoding(_) => write!(f, "StreamState::Decoding"),
            StreamState::Reading(_) => write!(f, "StreamState::Reading"),
            StreamState::Error => write!(f, "StreamState::Error"),
        }
    }
}

pub struct LiquidParquetRecordBatchStream {
    metadata: Arc<ParquetMetaData>,

    schema: SchemaRef,

    row_groups: VecDeque<usize>,

    projection: ProjectionMask,

    batch_size: usize,

    selection: Option<RowSelection>,

    /// This is an option so it can be moved into a future
    reader: Option<ReaderFactory>,

    state: StreamState,
}

impl std::fmt::Debug for LiquidParquetRecordBatchStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParquetRecordBatchStream")
            .field("metadata", &self.metadata)
            .field("schema", &self.schema)
            .field("batch_size", &self.batch_size)
            .field("projection", &self.projection)
            .field("state", &self.state)
            .finish()
    }
}

impl LiquidParquetRecordBatchStream {
    pub fn from_parquet(stream: ParquetRecordBatchStream<Box<dyn AsyncFileReader>>) -> Self {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            std::mem::transmute(stream)
        }
    }
}

impl Stream for LiquidParquetRecordBatchStream {
    type Item = Result<RecordBatch, parquet::errors::ParquetError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match &mut self.state {
                StreamState::Decoding(batch_reader) => match batch_reader.next() {
                    Some(Ok(batch)) => {
                        return Poll::Ready(Some(Ok(batch)));
                    }
                    Some(Err(e)) => {
                        self.state = StreamState::Error;
                        return Poll::Ready(Some(Err(ParquetError::ArrowError(e.to_string()))));
                    }
                    None => self.state = StreamState::Init,
                },
                StreamState::Init => {
                    let row_group_idx = match self.row_groups.pop_front() {
                        Some(idx) => idx,
                        None => return Poll::Ready(None),
                    };

                    let reader = self.reader.take().expect("lost reader");

                    let row_count = self.metadata.row_group(row_group_idx).num_rows() as usize;

                    let selection = self.selection.as_mut().map(|s| s.split_off(row_count));

                    let fut = reader
                        .read_row_group(
                            row_group_idx,
                            selection,
                            self.projection.clone(),
                            self.batch_size,
                        )
                        .boxed();

                    self.state = StreamState::Reading(fut)
                }
                StreamState::Reading(f) => match ready!(f.poll_unpin(cx)) {
                    Ok((reader_factory, maybe_reader)) => {
                        self.reader = Some(reader_factory);
                        match maybe_reader {
                            // Read records from [`ParquetRecordBatchReader`]
                            Some(reader) => self.state = StreamState::Decoding(reader),
                            // All rows skipped, read next row group
                            None => self.state = StreamState::Init,
                        }
                    }
                    Err(e) => {
                        self.state = StreamState::Error;
                        return Poll::Ready(Some(Err(e)));
                    }
                },
                StreamState::Error => return Poll::Ready(None), // Ends the stream as error happens.
            }
        }
    }
}
