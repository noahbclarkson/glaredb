use std::collections::HashMap;
use std::sync::Arc;

use crate::functions::table_location_and_opts;
use async_trait::async_trait;
use datafusion::arrow::array::{Int32Builder, Int64Builder, StringBuilder, UInt64Builder};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::{MemTable, TableProvider};
use datafusion_ext::errors::{ExtensionError, Result};
use datafusion_ext::functions::{FuncParamValue, TableFunc, TableFuncContextProvider};
use datasources::lake::iceberg::table::IcebergTable;
use datasources::lake::storage_options_into_object_store;
use protogen::metastore::types::catalog::RuntimePreference;

/// Scan an iceberg table.
#[derive(Debug, Clone, Copy)]
pub struct IcebergScan;

#[async_trait]
impl TableFunc for IcebergScan {
    fn runtime_preference(&self) -> RuntimePreference {
        // TODO: Detect runtime
        RuntimePreference::Remote
    }

    fn name(&self) -> &str {
        "iceberg_scan"
    }

    async fn create_provider(
        &self,
        ctx: &dyn TableFuncContextProvider,
        args: Vec<FuncParamValue>,
        mut opts: HashMap<String, FuncParamValue>,
    ) -> Result<Arc<dyn TableProvider>> {
        // TODO: Reduce duplication
        let (loc, opts) = table_location_and_opts(ctx, args, &mut opts)?;

        let store = storage_options_into_object_store(&loc, &opts).map_err(box_err)?;
        let table = IcebergTable::open(loc.clone(), store)
            .await
            .map_err(box_err)?;
        let reader = table.table_reader().await.map_err(box_err)?;

        Ok(reader)
    }
}

/// Scan snapshot information for an iceberg tables. Will not attempt to read
/// data files.
#[derive(Debug, Clone, Copy)]
pub struct IcebergSnapshots;

#[async_trait]
impl TableFunc for IcebergSnapshots {
    fn runtime_preference(&self) -> RuntimePreference {
        RuntimePreference::Remote
    }
    fn name(&self) -> &str {
        "iceberg_snapshots"
    }

    async fn create_provider(
        &self,
        ctx: &dyn TableFuncContextProvider,
        args: Vec<FuncParamValue>,
        mut opts: HashMap<String, FuncParamValue>,
    ) -> Result<Arc<dyn TableProvider>> {
        let (loc, opts) = table_location_and_opts(ctx, args, &mut opts)?;

        let store = storage_options_into_object_store(&loc, &opts).map_err(box_err)?;
        let table = IcebergTable::open(loc, store).await.map_err(box_err)?;

        let snapshots = &table.metadata().snapshots;

        let schema = Arc::new(Schema::new(vec![
            Field::new("snapshot_id", DataType::Int64, false),
            Field::new("timestamp_ms", DataType::Int64, false),
            Field::new("manifest_list", DataType::Utf8, false),
            Field::new("schema_id", DataType::Int32, false),
        ]));

        let mut snapshot_id = Int64Builder::new();
        let mut timestamp_ms = Int64Builder::new();
        let mut manifest_list = StringBuilder::new();
        let mut schema_id = Int32Builder::new();

        for snapshot in snapshots {
            snapshot_id.append_value(snapshot.snapshot_id);
            timestamp_ms.append_value(snapshot.timestamp_ms);
            manifest_list.append_value(&snapshot.manifest_list);
            schema_id.append_value(snapshot.schema_id);
        }

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(snapshot_id.finish()),
                Arc::new(timestamp_ms.finish()),
                Arc::new(manifest_list.finish()),
                Arc::new(schema_id.finish()),
            ],
        )?;

        Ok(Arc::new(
            MemTable::try_new(schema, vec![vec![batch]]).unwrap(),
        ))
    }
}

/// Scan data file metadata for the current snapshot of an iceberg table. Will
/// not attempt to read data files.
#[derive(Debug, Clone, Copy)]
pub struct IcebergDataFiles;

#[async_trait]
impl TableFunc for IcebergDataFiles {
    fn runtime_preference(&self) -> RuntimePreference {
        RuntimePreference::Remote
    }
    fn name(&self) -> &str {
        "iceberg_data_files"
    }

    async fn create_provider(
        &self,
        ctx: &dyn TableFuncContextProvider,
        args: Vec<FuncParamValue>,
        mut opts: HashMap<String, FuncParamValue>,
    ) -> Result<Arc<dyn TableProvider>> {
        let (loc, opts) = table_location_and_opts(ctx, args, &mut opts)?;

        let store = storage_options_into_object_store(&loc, &opts).map_err(box_err)?;
        let table = IcebergTable::open(loc, store).await.map_err(box_err)?;

        let manifests = table.read_manifests().await.map_err(box_err)?;

        let schema = Arc::new(Schema::new(vec![
            Field::new("manifest_index", DataType::UInt64, false),
            Field::new("manifest_content", DataType::Utf8, false),
            Field::new("snapshot_id", DataType::Int64, true),
            Field::new("sequence_number", DataType::Int64, true),
            Field::new("file_sequence_number", DataType::Int64, true),
            Field::new("file_path", DataType::Utf8, false),
            Field::new("file_format", DataType::Utf8, false),
            Field::new("record_count", DataType::Int64, false),
            Field::new("file_size_bytes", DataType::Int64, false),
        ]));

        let mut manifest_index = UInt64Builder::new();
        let mut manifest_content = StringBuilder::new();
        let mut snapshot_id = Int64Builder::new();
        let mut sequence_number = Int64Builder::new();
        let mut file_sequence_number = Int64Builder::new();
        let mut file_path = StringBuilder::new();
        let mut file_format = StringBuilder::new();
        let mut record_count = Int64Builder::new();
        let mut file_size_bytes = Int64Builder::new();

        for (idx, manifest) in manifests.into_iter().enumerate() {
            for entry in manifest.entries {
                // Manifest metadata
                manifest_index.append_value(idx as u64);
                manifest_content.append_value(manifest.metadata.content.to_string());

                // Entry data
                snapshot_id.append_option(entry.snapshot_id);
                sequence_number.append_option(entry.sequence_number);
                file_sequence_number.append_option(entry.file_sequence_number);
                file_path.append_value(&entry.data_file.file_path);
                file_format.append_value(&entry.data_file.file_format);
                record_count.append_value(entry.data_file.record_count);
                file_size_bytes.append_value(entry.data_file.file_size_in_bytes);
            }
        }

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(manifest_index.finish()),
                Arc::new(manifest_content.finish()),
                Arc::new(snapshot_id.finish()),
                Arc::new(sequence_number.finish()),
                Arc::new(file_sequence_number.finish()),
                Arc::new(file_path.finish()),
                Arc::new(file_format.finish()),
                Arc::new(record_count.finish()),
                Arc::new(file_size_bytes.finish()),
            ],
        )?;

        Ok(Arc::new(
            MemTable::try_new(schema, vec![vec![batch]]).unwrap(),
        ))
    }
}

fn box_err<E>(err: E) -> ExtensionError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ExtensionError::Access(Box::new(err))
}
