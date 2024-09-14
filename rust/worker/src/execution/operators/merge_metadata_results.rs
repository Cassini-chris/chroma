use crate::{
    execution::operator::Operator,
    segment::{
        record_segment::{RecordSegmentReader, RecordSegmentReaderCreationError},
        LogMaterializer, LogMaterializerError,
    },
};
use async_trait::async_trait;
use chroma_blockstore::provider::BlockfileProvider;
use chroma_error::{ChromaError, ErrorCodes};
use chroma_index::utils::{merge_sorted_vecs_conjunction, merge_sorted_vecs_disjunction};
use chroma_types::{
    Chunk, LogRecord, MaterializedLogOperation, Metadata, MetadataValueConversionError, Segment,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use thiserror::Error;
use tracing::{error, trace, Instrument, Span};

#[derive(Debug)]
pub struct MergeMetadataResultsOperator {}

impl MergeMetadataResultsOperator {
    pub fn new() -> Box<Self> {
        Box::new(MergeMetadataResultsOperator {})
    }
}

#[derive(Debug)]
pub struct MergeMetadataResultsOperatorInput {
    // Result of PullLogs.
    filtered_log: Chunk<LogRecord>,
    // Offset ids corresponding to the user ids asked for explicitly by the user.
    user_offset_ids: Option<Vec<u32>>,
    // The offset ids filtered by the where and where_document clause.
    filtered_offset_ids: Option<Vec<u32>>,
    record_segment_definition: Segment,
    blockfile_provider: BlockfileProvider,
    offset: Option<u32>,
    limit: Option<u32>,
    include_metadata: bool,
}

impl MergeMetadataResultsOperatorInput {
    pub fn new(
        filtered_log: Chunk<LogRecord>,
        user_offset_ids: Option<Vec<u32>>,
        filtered_offset_ids: Option<Vec<u32>>,
        record_segment_definition: Segment,
        blockfile_provider: BlockfileProvider,
        offset: Option<u32>,
        limit: Option<u32>,
        include_metadata: bool,
    ) -> Self {
        Self {
            filtered_log,
            user_offset_ids,
            filtered_offset_ids,
            record_segment_definition,
            blockfile_provider,
            offset,
            limit,
            include_metadata,
        }
    }
}

#[derive(Debug)]
pub struct MergeMetadataResultsOperatorOutput {
    pub ids: Vec<String>,
    pub metadata: Vec<Option<Metadata>>,
    pub documents: Vec<Option<String>>,
}

#[derive(Error, Debug)]
pub enum MergeMetadataResultsOperatorError {
    #[error("Error creating Record Segment")]
    RecordSegmentCreationError(#[from] RecordSegmentReaderCreationError),
    #[error("Error reading Record Segment")]
    RecordSegmentReadError,
    #[error("Error converting metadata")]
    MetadataConversionError(#[from] MetadataValueConversionError),
    #[error("Error materializing logs")]
    LogMaterializationError(#[from] LogMaterializerError),
}

impl ChromaError for MergeMetadataResultsOperatorError {
    fn code(&self) -> ErrorCodes {
        match self {
            MergeMetadataResultsOperatorError::RecordSegmentCreationError(e) => e.code(),
            MergeMetadataResultsOperatorError::RecordSegmentReadError => ErrorCodes::Internal,
            MergeMetadataResultsOperatorError::MetadataConversionError(e) => e.code(),
            MergeMetadataResultsOperatorError::LogMaterializationError(e) => e.code(),
        }
    }
}

#[async_trait]
impl Operator<MergeMetadataResultsOperatorInput, MergeMetadataResultsOperatorOutput>
    for MergeMetadataResultsOperator
{
    type Error = MergeMetadataResultsOperatorError;

    fn get_name(&self) -> &'static str {
        "MergeMetadataResultsOperator"
    }

    async fn run(
        &self,
        input: &MergeMetadataResultsOperatorInput,
    ) -> Result<MergeMetadataResultsOperatorOutput, Self::Error> {
        trace!(
            "[MergeMetadataResultsOperator] segment id: {}",
            input.record_segment_definition.id.to_string()
        );

        // Initialize record segment reader
        let record_segment_reader = match RecordSegmentReader::from_segment(
            &input.record_segment_definition,
            &input.blockfile_provider,
        )
        .await
        {
            Ok(reader) => Some(reader),
            // Uninitialized segment is fine and means that the record
            // segment is not yet initialized in storage.
            Err(e) if matches!(*e, RecordSegmentReaderCreationError::UninitializedSegment) => None,
            Err(e) => {
                tracing::error!("Error creating record segment reader {}", e);
                return Err(MergeMetadataResultsOperatorError::RecordSegmentCreationError(*e));
            }
        };

        // Materialize the logs.
        let materializer = LogMaterializer::new(
            record_segment_reader.clone(),
            input.filtered_log.clone(),
            None,
        );
        let mat_records = materializer
            .materialize()
            .instrument(tracing::trace_span!(parent: Span::current(), "Materialize logs"))
            .await
            .map_err(|e| {
                tracing::error!("Error materializing log: {}", e);
                MergeMetadataResultsOperatorError::LogMaterializationError(e)
            })?;

        // Merge the offset ids, assuming the user_offset_ids and filtered_offset_ids are ordered.
        let merged_oids_holder: Vec<u32>;
        let merged_offset_ids = match (&input.filtered_offset_ids, &input.user_offset_ids) {
            (Some(fids), Some(uids)) => {
                merged_oids_holder = merge_sorted_vecs_conjunction(fids, uids);
                &merged_oids_holder
            }
            (Some(oids), None) | (None, Some(oids)) => oids,
            // If both filter and user offset ids are None, it suggests user did not specify any filter. We fetch all offset ids using the record segment reader
            _ => {
                let live_log_offset_ids = mat_records
                    .iter()
                    .filter_map(|(log, _)| {
                        (log.final_operation != MaterializedLogOperation::DeleteExisting)
                            .then_some(log.offset_id)
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                merged_oids_holder = match &record_segment_reader {
                    Some(reader) => {
                        let compact_offset_ids =
                            reader.get_all_offset_ids().await.map_err(|e| {
                                tracing::error!("Error reading record segment: {}", e);
                                MergeMetadataResultsOperatorError::RecordSegmentReadError
                            })?;
                        merge_sorted_vecs_disjunction(&live_log_offset_ids, &compact_offset_ids)
                    }
                    None => live_log_offset_ids,
                };
                &merged_oids_holder
            }
        };

        // Truncate the offset ids using offset and limit
        let skip_count = input.offset.map(|o| o as usize).unwrap_or(0);
        let take_count = input
            .limit
            .map(|l| l as usize)
            .unwrap_or(merged_offset_ids.len());

        // Hydrate data
        let truncated_offset_ids = merged_offset_ids[skip_count..(skip_count + take_count)].iter();
        let truncated_offset_id_order: HashMap<u32, usize> = truncated_offset_ids
            .clone()
            .enumerate()
            .map(|(i, offset_id)| (*offset_id, i))
            .collect();
        let mut ids: Vec<String> = vec![String::new(); take_count];
        let mut metadata = Vec::new();
        let mut documents = Vec::new();
        let mut logged_offset_ids: HashSet<u32> = HashSet::new();
        if input.include_metadata {
            metadata = vec![None; take_count];
            documents = vec![None; take_count];
        }

        // Hydrate the data from the materialized logs first
        for (log, _) in mat_records.iter() {
            // It's important to account for the records that are
            // deleted also here so that we can subsequently ignore
            // them when reading the record segment.
            logged_offset_ids.insert(log.offset_id);
            if let Some(&index) = truncated_offset_id_order.get(&log.offset_id) {
                // Ids get pushed irrespective of whether metadata is included or not.
                ids[index] = log.merged_user_id();
                if input.include_metadata {
                    let final_metadata = log.merged_metadata();
                    metadata[index] = (!final_metadata.is_empty()).then_some(final_metadata);
                    documents[index] = log.merged_document();
                }
            }
        }

        // Hydrate the remaining data from the record segment
        if let Some(reader) = record_segment_reader {
            for (&offset_id, &index) in truncated_offset_id_order
                .iter()
                .filter(|(o, _)| !logged_offset_ids.contains(*o))
            {
                let user_id = reader
                    .get_user_id_for_offset_id(offset_id)
                    .await
                    .map_err(|e| {
                        tracing::error!("Error reading record segment: {}", e);
                        MergeMetadataResultsOperatorError::RecordSegmentReadError
                    })?;
                ids[index] = user_id.to_string();
                if input.include_metadata {
                    let record = reader
                        .get_data_for_offset_id(offset_id)
                        .await
                        .map_err(|e| {
                            tracing::error!("Error reading Record Segment: {}", e);
                            MergeMetadataResultsOperatorError::RecordSegmentReadError
                        })?;
                    metadata[index] = record.metadata;
                    documents[index] = record.document.map(str::to_string);
                }
            }
        }

        Ok(MergeMetadataResultsOperatorOutput {
            ids,
            metadata,
            documents,
        })
    }
}

#[cfg(test)]
mod test {
    use crate::{
        execution::{
            operator::Operator,
            operators::merge_metadata_results::{
                MergeMetadataResultsOperator, MergeMetadataResultsOperatorInput,
            },
        },
        segment::{
            metadata_segment::MetadataSegmentWriter,
            record_segment::{
                RecordSegmentReader, RecordSegmentReaderCreationError, RecordSegmentWriter,
            },
            LogMaterializer, SegmentFlusher, SegmentWriter,
        },
    };
    use chroma_blockstore::{
        arrow::{config::TEST_MAX_BLOCK_SIZE_BYTES, provider::ArrowBlockfileProvider},
        provider::BlockfileProvider,
    };
    use chroma_cache::{cache::Cache, config::CacheConfig, config::UnboundedCacheConfig};
    use chroma_storage::{local::LocalStorage, Storage};
    use chroma_types::{
        Chunk, LogRecord, MetadataValue, Operation, OperationRecord, UpdateMetadataValue,
    };
    use std::{collections::HashMap, str::FromStr};
    use uuid::Uuid;

    #[tokio::test]
    async fn test_merge_and_hydrate() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let storage = Storage::Local(LocalStorage::new(tmp_dir.path().to_str().unwrap()));
        let block_cache = Cache::new(&CacheConfig::Unbounded(UnboundedCacheConfig {}));
        let sparse_index_cache = Cache::new(&CacheConfig::Unbounded(UnboundedCacheConfig {}));
        let arrow_blockfile_provider = ArrowBlockfileProvider::new(
            storage,
            TEST_MAX_BLOCK_SIZE_BYTES,
            block_cache,
            sparse_index_cache,
        );
        let blockfile_provider =
            BlockfileProvider::ArrowBlockfileProvider(arrow_blockfile_provider);
        let mut record_segment = chroma_types::Segment {
            id: Uuid::from_str("00000000-0000-0000-0000-000000000000").expect("parse error"),
            r#type: chroma_types::SegmentType::BlockfileRecord,
            scope: chroma_types::SegmentScope::RECORD,
            collection: Uuid::from_str("00000000-0000-0000-0000-000000000000")
                .expect("parse error"),
            metadata: None,
            file_path: HashMap::new(),
        };
        let mut metadata_segment = chroma_types::Segment {
            id: Uuid::from_str("00000000-0000-0000-0000-000000000001").expect("parse error"),
            r#type: chroma_types::SegmentType::BlockfileMetadata,
            scope: chroma_types::SegmentScope::METADATA,
            collection: Uuid::from_str("00000000-0000-0000-0000-000000000000")
                .expect("parse error"),
            metadata: None,
            file_path: HashMap::new(),
        };
        {
            let segment_writer =
                RecordSegmentWriter::from_segment(&record_segment, &blockfile_provider)
                    .await
                    .expect("Error creating segment writer");
            let mut metadata_writer =
                MetadataSegmentWriter::from_segment(&metadata_segment, &blockfile_provider)
                    .await
                    .expect("Error creating segment writer");
            let mut update_metadata = HashMap::new();
            update_metadata.insert(
                String::from("hello"),
                UpdateMetadataValue::Str(String::from("world")),
            );
            update_metadata.insert(
                String::from("bye"),
                UpdateMetadataValue::Str(String::from("world")),
            );
            let data = vec![
                LogRecord {
                    log_offset: 1,
                    record: OperationRecord {
                        id: "embedding_id_1".to_string(),
                        embedding: Some(vec![1.0, 2.0, 3.0]),
                        encoding: None,
                        metadata: Some(update_metadata.clone()),
                        document: Some(String::from("This is a document about cats.")),
                        operation: Operation::Add,
                    },
                },
                LogRecord {
                    log_offset: 2,
                    record: OperationRecord {
                        id: "embedding_id_2".to_string(),
                        embedding: Some(vec![4.0, 5.0, 6.0]),
                        encoding: None,
                        metadata: Some(update_metadata),
                        document: Some(String::from("This is a document about dogs.")),
                        operation: Operation::Add,
                    },
                },
            ];
            let data: Chunk<LogRecord> = Chunk::new(data.into());
            let mut record_segment_reader: Option<RecordSegmentReader> = None;
            match RecordSegmentReader::from_segment(&record_segment, &blockfile_provider).await {
                Ok(reader) => {
                    record_segment_reader = Some(reader);
                }
                Err(e) => {
                    match *e {
                        // Uninitialized segment is fine and means that the record
                        // segment is not yet initialized in storage.
                        RecordSegmentReaderCreationError::UninitializedSegment => {
                            record_segment_reader = None;
                        }
                        RecordSegmentReaderCreationError::BlockfileOpenError(_) => {
                            panic!("Error creating record segment reader");
                        }
                        RecordSegmentReaderCreationError::InvalidNumberOfFiles => {
                            panic!("Error creating record segment reader");
                        }
                    };
                }
            };
            let materializer = LogMaterializer::new(record_segment_reader, data, None);
            let mat_records = materializer
                .materialize()
                .await
                .expect("Log materialization failed");
            metadata_writer
                .apply_materialized_log_chunk(mat_records.clone())
                .await
                .expect("Apply materialized log to metadata segment failed");
            segment_writer
                .apply_materialized_log_chunk(mat_records)
                .await
                .expect("Apply materialized log to record segment failed");
            metadata_writer
                .write_to_blockfiles()
                .await
                .expect("Metadata writer: write to blockfile failed");
            let record_flusher = segment_writer
                .commit()
                .expect("Commit for segment writer failed");
            let metadata_flusher = metadata_writer
                .commit()
                .expect("Commit for metadata writer failed");
            record_segment.file_path = record_flusher
                .flush()
                .await
                .expect("Flush record segment writer failed");
            metadata_segment.file_path = metadata_flusher
                .flush()
                .await
                .expect("Flush metadata segment writer failed");
        }
        let mut update_metadata = HashMap::new();
        update_metadata.insert(
            String::from("hello"),
            UpdateMetadataValue::Str(String::from("new_world")),
        );
        update_metadata.insert(
            String::from("hello_again"),
            UpdateMetadataValue::Str(String::from("new_world")),
        );
        let data = vec![
            LogRecord {
                log_offset: 3,
                record: OperationRecord {
                    id: "embedding_id_3".to_string(),
                    embedding: Some(vec![7.0, 8.0, 9.0]),
                    encoding: None,
                    metadata: Some(update_metadata.clone()),
                    document: Some(String::from("This is a document about dogs.")),
                    operation: Operation::Add,
                },
            },
            LogRecord {
                log_offset: 4,
                record: OperationRecord {
                    id: "embedding_id_1".to_string(),
                    embedding: None,
                    encoding: None,
                    metadata: Some(update_metadata.clone()),
                    document: None,
                    operation: Operation::Update,
                },
            },
        ];
        let data: Chunk<LogRecord> = Chunk::new(data.into());
        let op = MergeMetadataResultsOperator::new();
        let input = MergeMetadataResultsOperatorInput::new(
            data,
            Some(vec![1, 3]),
            Some(vec![1, 2, 3]),
            record_segment,
            blockfile_provider,
            None,
            None,
            true,
        );
        let output = op.run(&input).await.expect("Error running operator");
        assert_eq!(2, output.ids.len());
        let mut id_to_data: HashMap<
            &String,
            (
                &Option<String>,
                &Option<HashMap<String, chroma_types::MetadataValue>>,
            ),
        > = HashMap::new();
        id_to_data.insert(
            output.ids.get(0).expect("Not none key"),
            (
                output.documents.get(0).expect("Not none value"),
                output.metadata.get(0).expect("Not none value"),
            ),
        );
        id_to_data.insert(
            output.ids.get(1).expect("Not none key"),
            (
                output.documents.get(1).expect("Not none value"),
                output.metadata.get(1).expect("Not none value"),
            ),
        );
        let mut ids_sorted = output.ids.clone();
        ids_sorted.sort();
        assert_eq!(
            *ids_sorted.get(0).expect("Expected not none id"),
            String::from("embedding_id_1")
        );
        assert_eq!(
            *ids_sorted.get(1).expect("Expected not none id"),
            String::from("embedding_id_3")
        );
        assert_eq!(
            id_to_data.contains_key(&String::from("embedding_id_1")),
            true
        );
        assert_eq!(
            id_to_data.contains_key(&String::from("embedding_id_3")),
            true
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("bye"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello_again"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_3"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_3"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello_again"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .0
                .as_ref()
                .expect("Expected not none"),
            &String::from("This is a document about cats.")
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_3"))
                .as_ref()
                .expect("Expected value")
                .0
                .as_ref()
                .expect("Expected not none"),
            &String::from("This is a document about dogs.")
        );
    }

    #[tokio::test]
    async fn test_merge_and_hydrate_full_scan() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let storage = Storage::Local(LocalStorage::new(tmp_dir.path().to_str().unwrap()));
        let block_cache = Cache::new(&CacheConfig::Unbounded(UnboundedCacheConfig {}));
        let sparse_index_cache = Cache::new(&CacheConfig::Unbounded(UnboundedCacheConfig {}));
        let arrow_blockfile_provider = ArrowBlockfileProvider::new(
            storage,
            TEST_MAX_BLOCK_SIZE_BYTES,
            block_cache,
            sparse_index_cache,
        );
        let blockfile_provider =
            BlockfileProvider::ArrowBlockfileProvider(arrow_blockfile_provider);
        let mut record_segment = chroma_types::Segment {
            id: Uuid::from_str("00000000-0000-0000-0000-000000000000").expect("parse error"),
            r#type: chroma_types::SegmentType::BlockfileRecord,
            scope: chroma_types::SegmentScope::RECORD,
            collection: Uuid::from_str("00000000-0000-0000-0000-000000000000")
                .expect("parse error"),
            metadata: None,
            file_path: HashMap::new(),
        };
        let mut metadata_segment = chroma_types::Segment {
            id: Uuid::from_str("00000000-0000-0000-0000-000000000001").expect("parse error"),
            r#type: chroma_types::SegmentType::BlockfileMetadata,
            scope: chroma_types::SegmentScope::METADATA,
            collection: Uuid::from_str("00000000-0000-0000-0000-000000000000")
                .expect("parse error"),
            metadata: None,
            file_path: HashMap::new(),
        };
        {
            let segment_writer =
                RecordSegmentWriter::from_segment(&record_segment, &blockfile_provider)
                    .await
                    .expect("Error creating segment writer");
            let mut metadata_writer =
                MetadataSegmentWriter::from_segment(&metadata_segment, &blockfile_provider)
                    .await
                    .expect("Error creating segment writer");
            let mut update_metadata = HashMap::new();
            update_metadata.insert(
                String::from("hello"),
                UpdateMetadataValue::Str(String::from("world")),
            );
            update_metadata.insert(
                String::from("bye"),
                UpdateMetadataValue::Str(String::from("world")),
            );
            let data = vec![
                LogRecord {
                    log_offset: 1,
                    record: OperationRecord {
                        id: "embedding_id_1".to_string(),
                        embedding: Some(vec![1.0, 2.0, 3.0]),
                        encoding: None,
                        metadata: Some(update_metadata.clone()),
                        document: Some(String::from("This is a document about cats.")),
                        operation: Operation::Add,
                    },
                },
                LogRecord {
                    log_offset: 2,
                    record: OperationRecord {
                        id: "embedding_id_2".to_string(),
                        embedding: Some(vec![4.0, 5.0, 6.0]),
                        encoding: None,
                        metadata: Some(update_metadata),
                        document: Some(String::from("This is a document about dogs.")),
                        operation: Operation::Add,
                    },
                },
            ];
            let data: Chunk<LogRecord> = Chunk::new(data.into());
            let mut record_segment_reader: Option<RecordSegmentReader> = None;
            match RecordSegmentReader::from_segment(&record_segment, &blockfile_provider).await {
                Ok(reader) => {
                    record_segment_reader = Some(reader);
                }
                Err(e) => {
                    match *e {
                        // Uninitialized segment is fine and means that the record
                        // segment is not yet initialized in storage.
                        RecordSegmentReaderCreationError::UninitializedSegment => {
                            record_segment_reader = None;
                        }
                        RecordSegmentReaderCreationError::BlockfileOpenError(_) => {
                            panic!("Error creating record segment reader");
                        }
                        RecordSegmentReaderCreationError::InvalidNumberOfFiles => {
                            panic!("Error creating record segment reader");
                        }
                    };
                }
            };
            let materializer = LogMaterializer::new(record_segment_reader, data, None);
            let mat_records = materializer
                .materialize()
                .await
                .expect("Log materialization failed");
            metadata_writer
                .apply_materialized_log_chunk(mat_records.clone())
                .await
                .expect("Apply materialized log to metadata segment failed");
            segment_writer
                .apply_materialized_log_chunk(mat_records)
                .await
                .expect("Apply materialized log to record segment failed");
            metadata_writer
                .write_to_blockfiles()
                .await
                .expect("Metadata writer: write to blockfile failed");
            let record_flusher = segment_writer
                .commit()
                .expect("Commit for segment writer failed");
            let metadata_flusher = metadata_writer
                .commit()
                .expect("Commit for metadata writer failed");
            record_segment.file_path = record_flusher
                .flush()
                .await
                .expect("Flush record segment writer failed");
            metadata_segment.file_path = metadata_flusher
                .flush()
                .await
                .expect("Flush metadata segment writer failed");
        }
        let mut update_metadata = HashMap::new();
        update_metadata.insert(
            String::from("hello"),
            UpdateMetadataValue::Str(String::from("new_world")),
        );
        update_metadata.insert(
            String::from("hello_again"),
            UpdateMetadataValue::Str(String::from("new_world")),
        );
        let data = vec![
            LogRecord {
                log_offset: 3,
                record: OperationRecord {
                    id: "embedding_id_3".to_string(),
                    embedding: Some(vec![7.0, 8.0, 9.0]),
                    encoding: None,
                    metadata: Some(update_metadata.clone()),
                    document: Some(String::from("This is a document about dogs.")),
                    operation: Operation::Add,
                },
            },
            LogRecord {
                log_offset: 4,
                record: OperationRecord {
                    id: "embedding_id_1".to_string(),
                    embedding: None,
                    encoding: None,
                    metadata: Some(update_metadata.clone()),
                    document: None,
                    operation: Operation::Update,
                },
            },
        ];
        let data: Chunk<LogRecord> = Chunk::new(data.into());
        let op = MergeMetadataResultsOperator::new();
        let input = MergeMetadataResultsOperatorInput::new(
            data,
            None,
            None,
            record_segment,
            blockfile_provider,
            None,
            None,
            true,
        );
        let output = op.run(&input).await.expect("Error running operator");
        assert_eq!(3, output.ids.len());
        let mut id_to_data: HashMap<
            &String,
            (
                &Option<String>,
                &Option<HashMap<String, chroma_types::MetadataValue>>,
            ),
        > = HashMap::new();
        id_to_data.insert(
            output.ids.get(0).expect("Not none key"),
            (
                output.documents.get(0).expect("Not none value"),
                output.metadata.get(0).expect("Not none value"),
            ),
        );
        id_to_data.insert(
            output.ids.get(1).expect("Not none key"),
            (
                output.documents.get(1).expect("Not none value"),
                output.metadata.get(1).expect("Not none value"),
            ),
        );
        id_to_data.insert(
            output.ids.get(2).expect("Not none key"),
            (
                output.documents.get(2).expect("Not none value"),
                output.metadata.get(2).expect("Not none value"),
            ),
        );
        let mut ids_sorted = output.ids.clone();
        ids_sorted.sort();
        assert_eq!(
            *ids_sorted.get(0).expect("Expected not none id"),
            String::from("embedding_id_1")
        );
        assert_eq!(
            *ids_sorted.get(1).expect("Expected not none id"),
            String::from("embedding_id_2")
        );
        assert_eq!(
            *ids_sorted.get(2).expect("Expected not none id"),
            String::from("embedding_id_3")
        );
        assert_eq!(
            id_to_data.contains_key(&String::from("embedding_id_1")),
            true
        );
        assert_eq!(
            id_to_data.contains_key(&String::from("embedding_id_2")),
            true
        );
        assert_eq!(
            id_to_data.contains_key(&String::from("embedding_id_3")),
            true
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("bye"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello_again"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_2"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_2"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("bye"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_3"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_3"))
                .as_ref()
                .expect("Expected value")
                .1
                .as_ref()
                .expect("Expected not none")
                .get(&String::from("hello_again"))
                .expect("Expected key to be present"),
            &MetadataValue::Str(String::from("new_world"))
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_1"))
                .as_ref()
                .expect("Expected value")
                .0
                .as_ref()
                .expect("Expected not none"),
            &String::from("This is a document about cats.")
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_2"))
                .as_ref()
                .expect("Expected value")
                .0
                .as_ref()
                .expect("Expected not none"),
            &String::from("This is a document about dogs.")
        );
        assert_eq!(
            id_to_data
                .get(&String::from("embedding_id_3"))
                .as_ref()
                .expect("Expected value")
                .0
                .as_ref()
                .expect("Expected not none"),
            &String::from("This is a document about dogs.")
        );
    }
}
