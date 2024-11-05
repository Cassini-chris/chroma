use std::{cmp::Ordering, sync::atomic};

use chroma_error::{ChromaError, ErrorCodes};
use chroma_types::{MaterializedLogOperation, SignedRoaringBitmap};
use roaring::RoaringBitmap;
use thiserror::Error;
use tonic::async_trait;
use tracing::{trace, Instrument, Span};

use crate::{
    execution::operator::Operator,
    segment::{record_segment::RecordSegmentReader, LogMaterializer, LogMaterializerError},
};

use super::{
    fetch_log::FetchLogOutput,
    fetch_segment::{FetchSegmentError, FetchSegmentOutput},
};

#[derive(Clone, Debug)]
pub struct LimitOperator {
    pub skip: u32,
    pub fetch: Option<u32>,
}

#[derive(Debug)]
pub struct LimitInput {
    pub logs: FetchLogOutput,
    pub segments: FetchSegmentOutput,
    pub log_offset_ids: SignedRoaringBitmap,
    pub compact_offset_ids: SignedRoaringBitmap,
}

#[derive(Debug)]
pub struct LimitOutput {
    pub offset_ids: RoaringBitmap,
}

#[derive(Error, Debug)]
pub enum LimitError {
    #[error("Error processing fetch segment output: {0}")]
    FetchSegment(#[from] FetchSegmentError),
    #[error("Error materializing log: {0}")]
    LogMaterializer(#[from] LogMaterializerError),
    #[error("Error reading record segment: {0}")]
    RecordSegment(#[from] Box<dyn ChromaError>),
}

impl ChromaError for LimitError {
    fn code(&self) -> ErrorCodes {
        match self {
            LimitError::FetchSegment(e) => e.code(),
            LimitError::LogMaterializer(e) => e.code(),
            LimitError::RecordSegment(e) => e.code(),
        }
    }
}

// This struct aims to help scanning a number of elements from a fixed offset in the imaginary
// segment where the log is compacted and the element in the mask is ignored.
struct SkipScanner<'me> {
    log_offset_ids: &'me RoaringBitmap,
    record_segment: &'me RecordSegmentReader<'me>,
    mask: &'me RoaringBitmap,
}

impl<'me> SkipScanner<'me> {
    // Find the rank of the target offset id in the imaginary segment
    //
    // The rank of a target is the number of elements strictly less than it
    //
    // Alternatively, it's the index in the imaginary segment that it can be
    // inserted into while maintaining the order of the imaginary segment
    async fn joint_rank(&self, target: u32) -> Result<usize, LimitError> {
        // # of elements strictly less than target in materialized log
        let log_rank = self.log_offset_ids.rank(target) as usize
            - self.log_offset_ids.contains(target) as usize;
        // # of elements strictly less than target in the record segment
        let record_rank = self.record_segment.get_offset_id_rank(target).await?;
        // # of elements strictly less than target in the mask
        let mask_rank = self.mask.rank(target) as usize - self.mask.contains(target) as usize;
        Ok(log_rank + record_rank - mask_rank)
    }

    // Seek the starting index in log and record segment given the number of elements to skip
    // The implementation is a binary search based on [`std::slice::binary_search_by`]
    //
    // [`std::slice::binary_search_by`]: https://github.com/rust-lang/rust/blob/705cfe0e966399e061d64dd3661bfbc57553ed87/library/core/src/slice/mod.rs#L2731-L2827
    // Retrieval timestamp: Nov 1, 2024
    // Source commit hash: a0215d8e46aab41219dea0bb1cbaaf97dafe2f89
    // Source license: Apache-2.0 or MIT
    async fn seek_starting_index(&self, skip: usize) -> Result<(usize, usize), LimitError> {
        if skip == 0 {
            return Ok((0, 0));
        }

        let mut size = self
            .record_segment
            .get_current_max_offset_id()
            .load(atomic::Ordering::Relaxed)
            .max(self.log_offset_ids.max().unwrap_or(0));
        if size == 0 {
            return Ok((0, 0));
        }

        let mut base = 0;
        while size > 1 {
            let half = size / 2;
            let mid = base + half;

            let cmp = self.joint_rank(mid).await?.cmp(&skip);
            base = if cmp == Ordering::Greater { base } else { mid };
            size -= half;
        }

        Ok((
            self.log_offset_ids.rank(base) as usize - self.log_offset_ids.contains(base) as usize,
            self.record_segment.get_offset_id_rank(base).await?,
        ))
    }

    // Seek the start in the log and record segment, then scan for the specified number of offset ids
    async fn seek_and_scan(
        &self,
        skip: usize,
        mut fetch: usize,
    ) -> Result<RoaringBitmap, LimitError> {
        let record_count = self.record_segment.count().await?;
        let (mut log_index, mut record_index) = self.seek_starting_index(skip).await?;
        let mut merged_result = Vec::new();

        while fetch > 0 {
            let log_offset_id = self.log_offset_ids.select(log_index as u32);
            let record_offset_id = (record_index < record_count).then_some(
                self.record_segment
                    .get_offset_id_at_index(record_index)
                    .await?,
            );
            match (log_offset_id, record_offset_id) {
                (_, Some(oid)) if self.mask.contains(oid) => {
                    record_index += 1;
                    continue;
                }
                (Some(log_oid), Some(record_oid)) => {
                    if log_oid < record_oid {
                        merged_result.push(log_oid);
                        log_index += 1;
                    } else {
                        merged_result.push(record_oid);
                        record_index += 1;
                    }
                }
                (None, Some(oid)) => {
                    merged_result.push(oid);
                    record_index += 1;
                }
                (Some(oid), None) => {
                    merged_result.push(oid);
                    log_index += 1;
                }
                _ => {}
            };
            fetch -= 1;
        }

        Ok(RoaringBitmap::from_sorted_iter(merged_result)
            .expect("Merged offset ids should be sorted"))
    }
}

#[async_trait]
impl Operator<LimitInput, LimitOutput> for LimitOperator {
    type Error = LimitError;

    async fn run(&self, input: &LimitInput) -> Result<LimitOutput, LimitError> {
        trace!("[{}]: {:?}", self.get_name(), input);

        let record_segment_reader = input.segments.record_segment_reader().await?;

        // Materialize the filtered offset ids from the materialized log
        let mut materialized_log_offset_ids = match &input.log_offset_ids {
            SignedRoaringBitmap::Include(rbm) => rbm.clone(),
            SignedRoaringBitmap::Exclude(rbm) => {
                let materializer =
                    LogMaterializer::new(record_segment_reader.clone(), input.logs.clone(), None);
                let materialized_logs = materializer
                    .materialize()
                    .instrument(tracing::trace_span!(parent: Span::current(), "Materialize logs"))
                    .await?;

                let active_domain: RoaringBitmap = materialized_logs
                    .iter()
                    .filter_map(|(log, _)| {
                        (!matches!(
                            log.final_operation,
                            MaterializedLogOperation::DeleteExisting
                        ))
                        .then_some(log.offset_id)
                    })
                    .collect();
                active_domain - rbm
            }
        };

        // Materialize all filtered offset ids with the compact segment
        let materialized_offset_ids = match &input.compact_offset_ids {
            SignedRoaringBitmap::Include(rbm) => {
                let mut merged_oids = materialized_log_offset_ids | rbm;
                merged_oids.remove_smallest(self.skip as u64);
                if let Some(take_count) = self.fetch {
                    merged_oids.into_iter().take(take_count as usize).collect()
                } else {
                    merged_oids
                }
            }
            SignedRoaringBitmap::Exclude(rbm) => {
                if let Some(reader) = record_segment_reader {
                    let record_count = reader.count().await?;
                    let log_count = materialized_log_offset_ids.len() as usize;
                    let filter_match_count = log_count + record_count - rbm.len() as usize;
                    let truncated_skip = (self.skip as usize).min(filter_match_count);
                    let truncated_fetch = (self.fetch.unwrap_or(u32::MAX) as usize)
                        .min(filter_match_count - truncated_skip);

                    let skip_scanner = SkipScanner {
                        log_offset_ids: &materialized_log_offset_ids,
                        record_segment: &reader,
                        mask: rbm,
                    };
                    skip_scanner
                        .seek_and_scan(truncated_skip, truncated_fetch)
                        .await?
                } else {
                    materialized_log_offset_ids.remove_smallest(self.skip as u64);
                    if let Some(take_count) = self.fetch {
                        materialized_log_offset_ids
                            .into_iter()
                            .take(take_count as usize)
                            .collect()
                    } else {
                        materialized_log_offset_ids
                    }
                }
            }
        };

        Ok(LimitOutput {
            offset_ids: materialized_offset_ids,
        })
    }
}
