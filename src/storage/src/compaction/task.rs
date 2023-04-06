// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::fmt::{Debug, Formatter};

use common_base::readable_size::ReadableSize;
use common_telemetry::{debug, error};
use store_api::logstore::LogStore;
use store_api::storage::RegionId;

use crate::compaction::writer::build_sst_reader;
use crate::error::Result;
use crate::manifest::action::RegionEdit;
use crate::manifest::region::RegionManifest;
use crate::region::{RegionWriterRef, SharedDataRef};
use crate::schema::RegionSchemaRef;
use crate::sst::{
    AccessLayerRef, FileHandle, FileId, FileMeta, Level, Source, SstInfo, WriteOptions,
};
use crate::wal::Wal;

const MAX_PARALLEL_COMPACTION: usize = 8;

#[async_trait::async_trait]
pub trait CompactionTask: Debug + Send + Sync + 'static {
    async fn run(self) -> Result<()>;
}

pub struct CompactionTaskImpl<S: LogStore> {
    pub schema: RegionSchemaRef,
    pub sst_layer: AccessLayerRef,
    pub outputs: Vec<CompactionOutput>,
    pub writer: RegionWriterRef,
    pub shared_data: SharedDataRef,
    pub wal: Wal<S>,
    pub manifest: RegionManifest,
    pub expired_ssts: Vec<FileHandle>,
    pub sst_write_buffer_size: ReadableSize,
    pub compaction_time_window: Option<i64>,
}

impl<S: LogStore> Debug for CompactionTaskImpl<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactionTaskImpl")
            .field("region_name", &self.shared_data.name())
            .finish()
    }
}

impl<S: LogStore> Drop for CompactionTaskImpl<S> {
    fn drop(&mut self) {
        self.mark_files_compacting(false);
    }
}

impl<S: LogStore> CompactionTaskImpl<S> {
    /// Compacts inputs SSTs, returns `(output file, compacted input file)`.
    async fn merge_ssts(&mut self) -> Result<(HashSet<FileMeta>, HashSet<FileMeta>)> {
        let mut futs = Vec::with_capacity(self.outputs.len());
        let mut compacted_inputs = HashSet::new();
        let region_id = self.shared_data.id();
        for output in self.outputs.drain(..) {
            let schema = self.schema.clone();
            let sst_layer = self.sst_layer.clone();
            let sst_write_buffer_size = self.sst_write_buffer_size;
            compacted_inputs.extend(output.inputs.iter().map(FileHandle::meta));

            // TODO(hl): Maybe spawn to runtime to exploit in-job parallelism.
            futs.push(async move {
                output
                    .build(region_id, schema, sst_layer, sst_write_buffer_size)
                    .await
            });
        }

        let mut outputs = HashSet::with_capacity(futs.len());
        while !futs.is_empty() {
            let mut task_chunk = Vec::with_capacity(MAX_PARALLEL_COMPACTION);
            for _ in 0..MAX_PARALLEL_COMPACTION {
                if let Some(task) = futs.pop() {
                    task_chunk.push(task);
                }
            }
            let metas = futures::future::try_join_all(task_chunk).await?;
            outputs.extend(metas.into_iter().flatten());
        }

        let inputs = compacted_inputs.into_iter().collect();
        Ok((outputs, inputs))
    }

    /// Writes updated SST info into manifest.
    // TODO(etolbakov): we are not persisting inferred compaction_time_window (#1083)[https://github.com/GreptimeTeam/greptimedb/pull/1083]
    async fn write_manifest_and_apply(
        &self,
        output: HashSet<FileMeta>,
        input: HashSet<FileMeta>,
    ) -> Result<()> {
        let version = &self.shared_data.version_control;
        let region_version = version.metadata().version();

        let edit = RegionEdit {
            region_version,
            flushed_sequence: None,
            files_to_add: Vec::from_iter(output.into_iter()),
            files_to_remove: Vec::from_iter(input.into_iter()),
        };
        debug!(
            "Compacted region: {}, region edit: {:?}",
            version.metadata().name(),
            edit
        );
        self.writer
            .write_edit_and_apply(&self.wal, &self.shared_data, &self.manifest, edit, None)
            .await
    }

    /// Mark files are under compaction.
    fn mark_files_compacting(&self, compacting: bool) {
        for o in &self.outputs {
            for input in &o.inputs {
                input.mark_compacting(compacting);
            }
        }
    }
}

#[async_trait::async_trait]
impl<S: LogStore> CompactionTask for CompactionTaskImpl<S> {
    async fn run(mut self) -> Result<()> {
        self.mark_files_compacting(true);

        let (output, mut compacted) = self.merge_ssts().await.map_err(|e| {
            error!(e; "Failed to compact region: {}", self.shared_data.name());
            e
        })?;
        compacted.extend(self.expired_ssts.iter().map(FileHandle::meta));
        self.write_manifest_and_apply(output, compacted)
            .await
            .map_err(|e| {
                error!(e; "Failed to update region manifest: {}", self.shared_data.name());
                e
            })
    }
}

/// Many-to-many compaction can be decomposed to a many-to-one compaction from level n to level n+1
/// and a many-to-one compaction from level n+1 to level n+1.
#[derive(Debug)]
pub struct CompactionOutput {
    /// Compaction output file level.
    pub(crate) output_level: Level,
    /// The left bound of time bucket.
    pub(crate) bucket_bound: i64,
    /// Bucket duration in seconds.
    pub(crate) bucket: i64,
    /// Compaction input files.
    pub(crate) inputs: Vec<FileHandle>,
}

impl CompactionOutput {
    async fn build(
        &self,
        region_id: RegionId,
        schema: RegionSchemaRef,
        sst_layer: AccessLayerRef,
        sst_write_buffer_size: ReadableSize,
    ) -> Result<Option<FileMeta>> {
        let reader = build_sst_reader(
            schema,
            sst_layer.clone(),
            &self.inputs,
            self.bucket_bound,
            self.bucket_bound + self.bucket,
        )
        .await?;

        let output_file_id = FileId::random();
        let opts = WriteOptions {
            sst_write_buffer_size,
        };

        Ok(sst_layer
            .write_sst(output_file_id, Source::Reader(reader), &opts)
            .await?
            .map(
                |SstInfo {
                     time_range,
                     file_size,
                     ..
                 }| FileMeta {
                    region_id,
                    file_id: output_file_id,
                    time_range,
                    level: self.output_level,
                    file_size,
                },
            ))
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::compaction::task::CompactionTask;

    pub type CallbackRef = Arc<dyn Fn() + Send + Sync>;

    pub struct NoopCompactionTask {
        pub cbs: Vec<CallbackRef>,
    }

    impl Debug for NoopCompactionTask {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("storage::compaction::task::tests::NoopCompactionTask")
                .finish()
        }
    }

    #[async_trait::async_trait]
    impl CompactionTask for NoopCompactionTask {
        async fn run(self) -> Result<()> {
            for cb in &self.cbs {
                cb()
            }
            Ok(())
        }
    }
}