// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::cmp::{max, min};
use std::ffi::CString;
use std::sync::Arc;

use arrow::array::{new_null_array, RecordBatch, RecordBatchIterator, RecordBatchReader};
use arrow::compute::can_cast_types;
use arrow::error::ArrowError;
use arrow::ffi::FFI_ArrowSchema;
use arrow::ffi_stream::FFI_ArrowArrayStream;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::pyarrow::{PyArrowType, ToPyArrow};
use datafusion::arrow::util::pretty;
use datafusion::common::stats::Precision;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::common::{DFSchema, DataFusionError, Statistics, UnnestOptions};
use datafusion::config::{CsvOptions, TableParquetOptions};
use datafusion::dataframe::{DataFrame, DataFrameWriteOptions};
use datafusion::datasource::memory::DataSourceExec;
use datafusion::datasource::physical_plan::FileScanConfig;
use datafusion::datasource::source::DataSource;
use datafusion::datasource::TableProvider;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::parquet::basic::{BrotliLevel, Compression, GzipLevel, ZstdLevel};
use datafusion::physical_plan::{ExecutionPlan, ExecutionPlanProperties};
use datafusion::prelude::*;
use datafusion::sql::unparser::plan_to_sql;
use datafusion_proto::physical_plan::AsExecutionPlan;
use datafusion_proto::protobuf::PhysicalPlanNode;
use prost::Message;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedStr;
use pyo3::types::{PyBytes, PyCapsule, PyDict, PyString, PyTuple, PyTupleMethods};
use tokio::task::JoinHandle;

use crate::catalog::PyTable;
use crate::common::df_schema::PyDFSchema;
use crate::errors::{py_datafusion_err, PyDataFusionError};
use crate::expr::sort_expr::to_sort_expressions;
use crate::physical_plan::{codec, PyExecutionPlan};
use crate::record_batch::PyRecordBatchStream;
use crate::sql::logical::PyLogicalPlan;
use crate::utils::{get_tokio_runtime, validate_pycapsule, wait_for_future};
use crate::{
    errors::PyDataFusionResult,
    expr::{sort_expr::PySortExpr, PyExpr},
};

// https://github.com/apache/datafusion-python/pull/1016#discussion_r1983239116
// - we have not decided on the table_provider approach yet
// this is an interim implementation
#[pyclass(name = "TableProvider", module = "datafusion")]
pub struct PyTableProvider {
    provider: Arc<dyn TableProvider>,
}

impl PyTableProvider {
    pub fn new(provider: Arc<dyn TableProvider>) -> Self {
        Self { provider }
    }

    pub fn as_table(&self) -> PyTable {
        let table_provider: Arc<dyn TableProvider> = self.provider.clone();
        PyTable::new(table_provider)
    }
}

/// A PyDataFrame is a representation of a logical plan and an API to compose statements.
/// Use it to build a plan and `.collect()` to execute the plan and collect the result.
/// The actual execution of a plan runs natively on Rust and Arrow on a multi-threaded environment.
#[pyclass(name = "DataFrame", module = "datafusion", subclass)]
#[derive(Clone)]
pub struct PyDataFrame {
    df: Arc<DataFrame>,
}

impl PyDataFrame {
    /// creates a new PyDataFrame
    pub fn new(df: DataFrame) -> Self {
        Self { df: Arc::new(df) }
    }
}

#[pymethods]
impl PyDataFrame {
    /// Enable selection for `df[col]`, `df[col1, col2, col3]`, and `df[[col1, col2, col3]]`
    fn __getitem__(&self, key: Bound<'_, PyAny>) -> PyDataFusionResult<Self> {
        if let Ok(key) = key.extract::<PyBackedStr>() {
            // df[col]
            self.select_columns(vec![key])
        } else if let Ok(tuple) = key.downcast::<PyTuple>() {
            // df[col1, col2, col3]
            let keys = tuple
                .iter()
                .map(|item| item.extract::<PyBackedStr>())
                .collect::<PyResult<Vec<PyBackedStr>>>()?;
            self.select_columns(keys)
        } else if let Ok(keys) = key.extract::<Vec<PyBackedStr>>() {
            // df[[col1, col2, col3]]
            self.select_columns(keys)
        } else {
            let message = "DataFrame can only be indexed by string index or indices".to_string();
            Err(PyDataFusionError::Common(message))
        }
    }

    fn __repr__(&self, py: Python) -> PyDataFusionResult<String> {
        let df = self.df.as_ref().clone().limit(0, Some(10))?;
        let batches = wait_for_future(py, df.collect())?;
        let batches_as_string = pretty::pretty_format_batches(&batches);
        match batches_as_string {
            Ok(batch) => Ok(format!("DataFrame()\n{batch}")),
            Err(err) => Ok(format!("Error: {:?}", err.to_string())),
        }
    }

    fn _repr_html_(&self, py: Python) -> PyDataFusionResult<String> {
        let mut html_str = "<table border='1'>\n".to_string();

        let df = self.df.as_ref().clone().limit(0, Some(10))?;
        let batches = wait_for_future(py, df.collect())?;

        if batches.is_empty() {
            html_str.push_str("</table>\n");
            return Ok(html_str);
        }

        let schema = batches[0].schema();

        let mut header = Vec::new();
        for field in schema.fields() {
            header.push(format!("<th>{}</td>", field.name()));
        }
        let header_str = header.join("");
        html_str.push_str(&format!("<tr>{}</tr>\n", header_str));

        for batch in batches {
            let formatters = batch
                .columns()
                .iter()
                .map(|c| ArrayFormatter::try_new(c.as_ref(), &FormatOptions::default()))
                .map(|c| {
                    c.map_err(|e| PyValueError::new_err(format!("Error: {:?}", e.to_string())))
                })
                .collect::<Result<Vec<_>, _>>()?;

            for row in 0..batch.num_rows() {
                let mut cells = Vec::new();
                for formatter in &formatters {
                    cells.push(format!("<td>{}</td>", formatter.value(row)));
                }
                let row_str = cells.join("");
                html_str.push_str(&format!("<tr>{}</tr>\n", row_str));
            }
        }

        html_str.push_str("</table>\n");

        Ok(html_str)
    }

    /// Calculate summary statistics for a DataFrame
    fn describe(&self, py: Python) -> PyDataFusionResult<Self> {
        let df = self.df.as_ref().clone();
        let stat_df = wait_for_future(py, df.describe())?;
        Ok(Self::new(stat_df))
    }

    /// Returns the schema from the logical plan
    fn schema(&self) -> PyArrowType<Schema> {
        PyArrowType(self.df.schema().into())
    }

    /// Convert this DataFrame into a Table that can be used in register_table
    /// By convention, into_... methods consume self and return the new object.
    /// Disabling the clippy lint, so we can use &self
    /// because we're working with Python bindings
    /// where objects are shared
    /// https://github.com/apache/datafusion-python/pull/1016#discussion_r1983239116
    /// - we have not decided on the table_provider approach yet
    #[allow(clippy::wrong_self_convention)]
    fn into_view(&self) -> PyDataFusionResult<PyTable> {
        // Call the underlying Rust DataFrame::into_view method.
        // Note that the Rust method consumes self; here we clone the inner Arc<DataFrame>
        // so that we don’t invalidate this PyDataFrame.
        let table_provider = self.df.as_ref().clone().into_view();
        let table_provider = PyTableProvider::new(table_provider);

        Ok(table_provider.as_table())
    }

    #[pyo3(signature = (*args))]
    fn select_columns(&self, args: Vec<PyBackedStr>) -> PyDataFusionResult<Self> {
        let args = args.iter().map(|s| s.as_ref()).collect::<Vec<&str>>();
        let df = self.df.as_ref().clone().select_columns(&args)?;
        Ok(Self::new(df))
    }

    #[pyo3(signature = (*args))]
    fn select(&self, args: Vec<PyExpr>) -> PyDataFusionResult<Self> {
        let expr = args.into_iter().map(|e| e.into()).collect();
        let df = self.df.as_ref().clone().select(expr)?;
        Ok(Self::new(df))
    }

    #[pyo3(signature = (*args))]
    fn drop(&self, args: Vec<PyBackedStr>) -> PyDataFusionResult<Self> {
        let cols = args.iter().map(|s| s.as_ref()).collect::<Vec<&str>>();
        let df = self.df.as_ref().clone().drop_columns(&cols)?;
        Ok(Self::new(df))
    }

    fn filter(&self, predicate: PyExpr) -> PyDataFusionResult<Self> {
        let df = self.df.as_ref().clone().filter(predicate.into())?;
        Ok(Self::new(df))
    }

    fn with_column(&self, name: &str, expr: PyExpr) -> PyDataFusionResult<Self> {
        let df = self.df.as_ref().clone().with_column(name, expr.into())?;
        Ok(Self::new(df))
    }

    fn with_columns(&self, exprs: Vec<PyExpr>) -> PyDataFusionResult<Self> {
        let mut df = self.df.as_ref().clone();
        for expr in exprs {
            let expr: Expr = expr.into();
            let name = format!("{}", expr.schema_name());
            df = df.with_column(name.as_str(), expr)?
        }
        Ok(Self::new(df))
    }

    /// Rename one column by applying a new projection. This is a no-op if the column to be
    /// renamed does not exist.
    fn with_column_renamed(&self, old_name: &str, new_name: &str) -> PyDataFusionResult<Self> {
        let df = self
            .df
            .as_ref()
            .clone()
            .with_column_renamed(old_name, new_name)?;
        Ok(Self::new(df))
    }

    fn aggregate(&self, group_by: Vec<PyExpr>, aggs: Vec<PyExpr>) -> PyDataFusionResult<Self> {
        let group_by = group_by.into_iter().map(|e| e.into()).collect();
        let aggs = aggs.into_iter().map(|e| e.into()).collect();
        let df = self.df.as_ref().clone().aggregate(group_by, aggs)?;
        Ok(Self::new(df))
    }

    #[pyo3(signature = (*exprs))]
    fn sort(&self, exprs: Vec<PySortExpr>) -> PyDataFusionResult<Self> {
        let exprs = to_sort_expressions(exprs);
        let df = self.df.as_ref().clone().sort(exprs)?;
        Ok(Self::new(df))
    }

    #[pyo3(signature = (count, offset=0))]
    fn limit(&self, count: usize, offset: usize) -> PyDataFusionResult<Self> {
        let df = self.df.as_ref().clone().limit(offset, Some(count))?;
        Ok(Self::new(df))
    }

    /// Executes the plan, returning a list of `RecordBatch`es.
    /// Unless some order is specified in the plan, there is no
    /// guarantee of the order of the result.
    fn collect(&self, py: Python) -> PyResult<Vec<PyObject>> {
        let batches = wait_for_future(py, self.df.as_ref().clone().collect())
            .map_err(PyDataFusionError::from)?;
        // cannot use PyResult<Vec<RecordBatch>> return type due to
        // https://github.com/PyO3/pyo3/issues/1813
        batches.into_iter().map(|rb| rb.to_pyarrow(py)).collect()
    }

    /// Cache DataFrame.
    fn cache(&self, py: Python) -> PyDataFusionResult<Self> {
        let df = wait_for_future(py, self.df.as_ref().clone().cache())?;
        Ok(Self::new(df))
    }

    /// Executes this DataFrame and collects all results into a vector of vector of RecordBatch
    /// maintaining the input partitioning.
    fn collect_partitioned(&self, py: Python) -> PyResult<Vec<Vec<PyObject>>> {
        let batches = wait_for_future(py, self.df.as_ref().clone().collect_partitioned())
            .map_err(PyDataFusionError::from)?;

        batches
            .into_iter()
            .map(|rbs| rbs.into_iter().map(|rb| rb.to_pyarrow(py)).collect())
            .collect()
    }

    /// Print the result, 20 lines by default
    #[pyo3(signature = (num=20))]
    fn show(&self, py: Python, num: usize) -> PyDataFusionResult<()> {
        let df = self.df.as_ref().clone().limit(0, Some(num))?;
        print_dataframe(py, df)
    }

    /// Filter out duplicate rows
    fn distinct(&self) -> PyDataFusionResult<Self> {
        let df = self.df.as_ref().clone().distinct()?;
        Ok(Self::new(df))
    }

    fn join(
        &self,
        right: PyDataFrame,
        how: &str,
        left_on: Vec<PyBackedStr>,
        right_on: Vec<PyBackedStr>,
    ) -> PyDataFusionResult<Self> {
        let join_type = match how {
            "inner" => JoinType::Inner,
            "left" => JoinType::Left,
            "right" => JoinType::Right,
            "full" => JoinType::Full,
            "semi" => JoinType::LeftSemi,
            "anti" => JoinType::LeftAnti,
            how => {
                return Err(PyDataFusionError::Common(format!(
                    "The join type {how} does not exist or is not implemented"
                )));
            }
        };

        let left_keys = left_on.iter().map(|s| s.as_ref()).collect::<Vec<&str>>();
        let right_keys = right_on.iter().map(|s| s.as_ref()).collect::<Vec<&str>>();

        let df = self.df.as_ref().clone().join(
            right.df.as_ref().clone(),
            join_type,
            &left_keys,
            &right_keys,
            None,
        )?;
        Ok(Self::new(df))
    }

    fn join_on(
        &self,
        right: PyDataFrame,
        on_exprs: Vec<PyExpr>,
        how: &str,
    ) -> PyDataFusionResult<Self> {
        let join_type = match how {
            "inner" => JoinType::Inner,
            "left" => JoinType::Left,
            "right" => JoinType::Right,
            "full" => JoinType::Full,
            "semi" => JoinType::LeftSemi,
            "anti" => JoinType::LeftAnti,
            how => {
                return Err(PyDataFusionError::Common(format!(
                    "The join type {how} does not exist or is not implemented"
                )));
            }
        };
        let exprs: Vec<Expr> = on_exprs.into_iter().map(|e| e.into()).collect();

        let df = self
            .df
            .as_ref()
            .clone()
            .join_on(right.df.as_ref().clone(), join_type, exprs)?;
        Ok(Self::new(df))
    }

    /// Print the query plan
    #[pyo3(signature = (verbose=false, analyze=false))]
    fn explain(&self, py: Python, verbose: bool, analyze: bool) -> PyDataFusionResult<()> {
        let df = self.df.as_ref().clone().explain(verbose, analyze)?;
        print_dataframe(py, df)
    }

    /// Get the logical plan for this `DataFrame`
    fn logical_plan(&self) -> PyResult<PyLogicalPlan> {
        Ok(self.df.as_ref().clone().logical_plan().clone().into())
    }

    /// Get the optimized logical plan for this `DataFrame`
    fn optimized_logical_plan(&self) -> PyDataFusionResult<PyLogicalPlan> {
        Ok(self.df.as_ref().clone().into_optimized_plan()?.into())
    }

    /// Get the execution plan for this `DataFrame`
    fn execution_plan(&self, py: Python) -> PyDataFusionResult<PyExecutionPlan> {
        let plan = wait_for_future(py, self.df.as_ref().clone().create_physical_plan())?;
        Ok(plan.into())
    }

    /// Repartition a `DataFrame` based on a logical partitioning scheme.
    fn repartition(&self, num: usize) -> PyDataFusionResult<Self> {
        let new_df = self
            .df
            .as_ref()
            .clone()
            .repartition(Partitioning::RoundRobinBatch(num))?;
        Ok(Self::new(new_df))
    }

    /// Repartition a `DataFrame` based on a logical partitioning scheme.
    #[pyo3(signature = (*args, num))]
    fn repartition_by_hash(&self, args: Vec<PyExpr>, num: usize) -> PyDataFusionResult<Self> {
        let expr = args.into_iter().map(|py_expr| py_expr.into()).collect();
        let new_df = self
            .df
            .as_ref()
            .clone()
            .repartition(Partitioning::Hash(expr, num))?;
        Ok(Self::new(new_df))
    }

    /// Calculate the union of two `DataFrame`s, preserving duplicate rows.The
    /// two `DataFrame`s must have exactly the same schema
    #[pyo3(signature = (py_df, distinct=false))]
    fn union(&self, py_df: PyDataFrame, distinct: bool) -> PyDataFusionResult<Self> {
        let new_df = if distinct {
            self.df
                .as_ref()
                .clone()
                .union_distinct(py_df.df.as_ref().clone())?
        } else {
            self.df.as_ref().clone().union(py_df.df.as_ref().clone())?
        };

        Ok(Self::new(new_df))
    }

    /// Calculate the distinct union of two `DataFrame`s.  The
    /// two `DataFrame`s must have exactly the same schema
    fn union_distinct(&self, py_df: PyDataFrame) -> PyDataFusionResult<Self> {
        let new_df = self
            .df
            .as_ref()
            .clone()
            .union_distinct(py_df.df.as_ref().clone())?;
        Ok(Self::new(new_df))
    }

    #[pyo3(signature = (column, preserve_nulls=true))]
    fn unnest_column(&self, column: &str, preserve_nulls: bool) -> PyDataFusionResult<Self> {
        // TODO: expose RecursionUnnestOptions
        // REF: https://github.com/apache/datafusion/pull/11577
        let unnest_options = UnnestOptions::default().with_preserve_nulls(preserve_nulls);
        let df = self
            .df
            .as_ref()
            .clone()
            .unnest_columns_with_options(&[column], unnest_options)?;
        Ok(Self::new(df))
    }

    #[pyo3(signature = (columns, preserve_nulls=true))]
    fn unnest_columns(
        &self,
        columns: Vec<String>,
        preserve_nulls: bool,
    ) -> PyDataFusionResult<Self> {
        // TODO: expose RecursionUnnestOptions
        // REF: https://github.com/apache/datafusion/pull/11577
        let unnest_options = UnnestOptions::default().with_preserve_nulls(preserve_nulls);
        let cols = columns.iter().map(|s| s.as_ref()).collect::<Vec<&str>>();
        let df = self
            .df
            .as_ref()
            .clone()
            .unnest_columns_with_options(&cols, unnest_options)?;
        Ok(Self::new(df))
    }

    /// Calculate the intersection of two `DataFrame`s.  The two `DataFrame`s must have exactly the same schema
    fn intersect(&self, py_df: PyDataFrame) -> PyDataFusionResult<Self> {
        let new_df = self
            .df
            .as_ref()
            .clone()
            .intersect(py_df.df.as_ref().clone())?;
        Ok(Self::new(new_df))
    }

    /// Calculate the exception of two `DataFrame`s.  The two `DataFrame`s must have exactly the same schema
    fn except_all(&self, py_df: PyDataFrame) -> PyDataFusionResult<Self> {
        let new_df = self.df.as_ref().clone().except(py_df.df.as_ref().clone())?;
        Ok(Self::new(new_df))
    }

    /// Write a `DataFrame` to a CSV file.
    fn write_csv(&self, path: &str, with_header: bool, py: Python) -> PyDataFusionResult<()> {
        let csv_options = CsvOptions {
            has_header: Some(with_header),
            ..Default::default()
        };
        wait_for_future(
            py,
            self.df.as_ref().clone().write_csv(
                path,
                DataFrameWriteOptions::new(),
                Some(csv_options),
            ),
        )?;
        Ok(())
    }

    /// Write a `DataFrame` to a Parquet file.
    #[pyo3(signature = (
        path,
        compression="zstd",
        compression_level=None
        ))]
    fn write_parquet(
        &self,
        path: &str,
        compression: &str,
        compression_level: Option<u32>,
        py: Python,
    ) -> PyDataFusionResult<()> {
        fn verify_compression_level(cl: Option<u32>) -> Result<u32, PyErr> {
            cl.ok_or(PyValueError::new_err("compression_level is not defined"))
        }

        let _validated = match compression.to_lowercase().as_str() {
            "snappy" => Compression::SNAPPY,
            "gzip" => Compression::GZIP(
                GzipLevel::try_new(compression_level.unwrap_or(6))
                    .map_err(|e| PyValueError::new_err(format!("{e}")))?,
            ),
            "brotli" => Compression::BROTLI(
                BrotliLevel::try_new(verify_compression_level(compression_level)?)
                    .map_err(|e| PyValueError::new_err(format!("{e}")))?,
            ),
            "zstd" => Compression::ZSTD(
                ZstdLevel::try_new(verify_compression_level(compression_level)? as i32)
                    .map_err(|e| PyValueError::new_err(format!("{e}")))?,
            ),
            "lzo" => Compression::LZO,
            "lz4" => Compression::LZ4,
            "lz4_raw" => Compression::LZ4_RAW,
            "uncompressed" => Compression::UNCOMPRESSED,
            _ => {
                return Err(PyDataFusionError::Common(format!(
                    "Unrecognized compression type {compression}"
                )));
            }
        };

        let mut compression_string = compression.to_string();
        if let Some(level) = compression_level {
            compression_string.push_str(&format!("({level})"));
        }

        let mut options = TableParquetOptions::default();
        options.global.compression = Some(compression_string);

        wait_for_future(
            py,
            self.df.as_ref().clone().write_parquet(
                path,
                DataFrameWriteOptions::new(),
                Option::from(options),
            ),
        )?;
        Ok(())
    }

    /// Executes a query and writes the results to a partitioned JSON file.
    fn write_json(&self, path: &str, py: Python) -> PyDataFusionResult<()> {
        wait_for_future(
            py,
            self.df
                .as_ref()
                .clone()
                .write_json(path, DataFrameWriteOptions::new(), None),
        )?;
        Ok(())
    }

    /// Convert to Arrow Table
    /// Collect the batches and pass to Arrow Table
    fn to_arrow_table(&self, py: Python<'_>) -> PyResult<PyObject> {
        let batches = self.collect(py)?.into_pyobject(py)?;
        let schema = self.schema().into_pyobject(py)?;

        // Instantiate pyarrow Table object and use its from_batches method
        let table_class = py.import("pyarrow")?.getattr("Table")?;
        let args = PyTuple::new(py, &[batches, schema])?;
        let table: PyObject = table_class.call_method1("from_batches", args)?.into();
        Ok(table)
    }

    #[pyo3(signature = (requested_schema=None))]
    fn __arrow_c_stream__<'py>(
        &'py mut self,
        py: Python<'py>,
        requested_schema: Option<Bound<'py, PyCapsule>>,
    ) -> PyDataFusionResult<Bound<'py, PyCapsule>> {
        let mut batches = wait_for_future(py, self.df.as_ref().clone().collect())?;
        let mut schema: Schema = self.df.schema().to_owned().into();

        if let Some(schema_capsule) = requested_schema {
            validate_pycapsule(&schema_capsule, "arrow_schema")?;

            let schema_ptr = unsafe { schema_capsule.reference::<FFI_ArrowSchema>() };
            let desired_schema = Schema::try_from(schema_ptr)?;

            schema = project_schema(schema, desired_schema)?;

            batches = batches
                .into_iter()
                .map(|record_batch| record_batch_into_schema(record_batch, &schema))
                .collect::<Result<Vec<RecordBatch>, ArrowError>>()?;
        }

        let batches_wrapped = batches.into_iter().map(Ok);

        let reader = RecordBatchIterator::new(batches_wrapped, Arc::new(schema));
        let reader: Box<dyn RecordBatchReader + Send> = Box::new(reader);

        let ffi_stream = FFI_ArrowArrayStream::new(reader);
        let stream_capsule_name = CString::new("arrow_array_stream").unwrap();
        PyCapsule::new(py, ffi_stream, Some(stream_capsule_name)).map_err(PyDataFusionError::from)
    }

    fn execute_stream(&self, py: Python) -> PyDataFusionResult<PyRecordBatchStream> {
        // create a Tokio runtime to run the async code
        let rt = &get_tokio_runtime().0;
        let df = self.df.as_ref().clone();
        let fut: JoinHandle<datafusion::common::Result<SendableRecordBatchStream>> =
            rt.spawn(async move { df.execute_stream().await });
        let stream = wait_for_future(py, fut).map_err(py_datafusion_err)?;
        Ok(PyRecordBatchStream::new(stream?))
    }

    fn execute_stream_partitioned(&self, py: Python) -> PyResult<Vec<PyRecordBatchStream>> {
        // create a Tokio runtime to run the async code
        let rt = &get_tokio_runtime().0;
        let df = self.df.as_ref().clone();
        let fut: JoinHandle<datafusion::common::Result<Vec<SendableRecordBatchStream>>> =
            rt.spawn(async move { df.execute_stream_partitioned().await });
        let stream = wait_for_future(py, fut).map_err(py_datafusion_err)?;

        match stream {
            Ok(batches) => Ok(batches.into_iter().map(PyRecordBatchStream::new).collect()),
            _ => Err(PyValueError::new_err(
                "Unable to execute stream partitioned",
            )),
        }
    }

    /// Convert to pandas dataframe with pyarrow
    /// Collect the batches, pass to Arrow Table & then convert to Pandas DataFrame
    fn to_pandas(&self, py: Python<'_>) -> PyResult<PyObject> {
        let table = self.to_arrow_table(py)?;

        // See also: https://arrow.apache.org/docs/python/generated/pyarrow.Table.html#pyarrow.Table.to_pandas
        let result = table.call_method0(py, "to_pandas")?;
        Ok(result)
    }

    /// Convert to Python list using pyarrow
    /// Each list item represents one row encoded as dictionary
    fn to_pylist(&self, py: Python<'_>) -> PyResult<PyObject> {
        let table = self.to_arrow_table(py)?;

        // See also: https://arrow.apache.org/docs/python/generated/pyarrow.Table.html#pyarrow.Table.to_pylist
        let result = table.call_method0(py, "to_pylist")?;
        Ok(result)
    }

    /// Convert to Python dictionary using pyarrow
    /// Each dictionary key is a column and the dictionary value represents the column values
    fn to_pydict(&self, py: Python) -> PyResult<PyObject> {
        let table = self.to_arrow_table(py)?;

        // See also: https://arrow.apache.org/docs/python/generated/pyarrow.Table.html#pyarrow.Table.to_pydict
        let result = table.call_method0(py, "to_pydict")?;
        Ok(result)
    }

    /// Convert to polars dataframe with pyarrow
    /// Collect the batches, pass to Arrow Table & then convert to polars DataFrame
    fn to_polars(&self, py: Python<'_>) -> PyResult<PyObject> {
        let table = self.to_arrow_table(py)?;
        let dataframe = py.import("polars")?.getattr("DataFrame")?;
        let args = PyTuple::new(py, &[table])?;
        let result: PyObject = dataframe.call1(args)?.into();
        Ok(result)
    }

    // Executes this DataFrame to get the total number of rows.
    fn count(&self, py: Python) -> PyDataFusionResult<usize> {
        Ok(wait_for_future(py, self.df.as_ref().clone().count())?)
    }

    fn distributed_plan(&self, py: Python<'_>) -> PyResult<DistributedPlan> {
        let future_plan = DistributedPlan::try_new(self.df.as_ref());
        wait_for_future(py, future_plan).map_err(py_datafusion_err)
    }

    fn plan_sql(&self, py: Python<'_>) -> PyResult<PyObject> {
        let logical_plan = self.df.logical_plan();

        let sql = plan_to_sql(logical_plan).map_err(py_datafusion_err)?;
        Ok(PyString::new(py, sql.to_string().as_ref()).into())
    }
}

#[pyclass(get_all)]
#[derive(Debug, Clone)]
pub struct DistributedPlan {
    min_size: usize,
    physical_plan: PyExecutionPlan,
}

#[pymethods]
impl DistributedPlan {
    #[new]
    fn unmarshal(state: Bound<PyDict>) -> PyResult<Self> {
        let ctx = SessionContext::new();
        let serialized_plan = state
            .get_item("plan")?
            .expect("missing key `plan` from state");
        let serialized_plan = serialized_plan.downcast::<PyBytes>()?.as_bytes();
        let min_size = state
            .get_item("min_size")?
            .expect("missing key `min_size` from state")
            .extract::<usize>()?;
        let plan = deserialize_plan(serialized_plan, &ctx)?;
        Ok(Self {
            min_size,
            physical_plan: PyExecutionPlan::new(plan),
        })
    }

    fn partition_count(&self) -> usize {
        self.physical_plan.partition_count()
    }

    fn num_bytes(&self) -> Option<usize> {
        self.stats_field(|stats| stats.total_byte_size)
    }

    fn num_rows(&self) -> Option<usize> {
        self.stats_field(|stats| stats.num_rows)
    }

    fn schema(&self) -> PyResult<PyDFSchema> {
        DFSchema::try_from(self.plan().schema())
            .map(PyDFSchema::from)
            .map_err(py_datafusion_err)
    }

    fn set_desired_parallelism(&mut self, desired_parallelism: usize) -> PyResult<()> {
        let updated_plan = self
            .plan()
            .clone()
            .transform_up(|node| {
                if let Some(exec) = node.as_any().downcast_ref::<DataSourceExec>() {
                    // Remove redundant ranges from partition files because FileScanConfig refuses to repartition
                    // if any file has a range defined (even when the range actually covers the entire file).
                    // The EnforceDistribution optimizer rule adds ranges for both full and partial files,
                    // so this tries to revert that in order to trigger a repartition when no files are actually split.
                    if let Some(file_scan) =
                        exec.data_source().as_any().downcast_ref::<FileScanConfig>()
                    {
                        let mut range_free_file_scan = file_scan.clone();
                        let mut total_size: usize = 0;
                        for group in range_free_file_scan.file_groups.iter_mut() {
                            for file in group.iter_mut() {
                                if let Some(range) = &file.range {
                                    total_size += (range.end - range.start) as usize;
                                    if range.start == 0 && range.end == file.object_meta.size as i64
                                    {
                                        file.range = None; // remove redundant range
                                    }
                                } else {
                                    total_size += file.object_meta.size;
                                }
                            }
                        }
                        let min_size_buckets = max(1, total_size.div_ceil(self.min_size));
                        let partitions = min(min_size_buckets, desired_parallelism);
                        let ordering = range_free_file_scan.eq_properties().output_ordering();
                        if let Some(repartitioned) =
                            range_free_file_scan.repartitioned(partitions, 1, ordering)?
                        {
                            return Ok(Transformed::yes(Arc::new(DataSourceExec::new(
                                repartitioned,
                            ))));
                        }
                    }
                }
                Ok(Transformed::no(node))
            })
            .map_err(py_datafusion_err)?
            .data;
        self.physical_plan = PyExecutionPlan::new(updated_plan);
        Ok(())
    }
}

impl DistributedPlan {
    async fn try_new(df: &DataFrame) -> Result<Self, DataFusionError> {
        let (mut session_state, logical_plan) = df.clone().into_parts();
        let min_size = session_state
            .config_options()
            .optimizer
            .repartition_file_min_size;
        // Create the physical plan with a single partition, to ensure that no files are split into ranges.
        // Otherwise, any subsequent repartition attempt would fail (see the comment in `set_desired_parallelism`)
        session_state
            .config_mut()
            .options_mut()
            .execution
            .target_partitions = 1;
        let physical_plan = session_state.create_physical_plan(&logical_plan).await?;
        let physical_plan = PyExecutionPlan::new(physical_plan);
        Ok(Self {
            min_size,
            physical_plan,
        })
    }

    fn plan(&self) -> &Arc<dyn ExecutionPlan> {
        &self.physical_plan.plan
    }

    fn stats_field(&self, field: fn(Statistics) -> Precision<usize>) -> Option<usize> {
        if let Ok(stats) = self.plan().statistics() {
            match field(stats) {
                Precision::Exact(n) => Some(n),
                _ => None,
            }
        } else {
            None
        }
    }
}

fn deserialize_plan(
    serialized_plan: &[u8],
    ctx: &SessionContext,
) -> PyResult<Arc<dyn ExecutionPlan>> {
    deltalake::ensure_initialized();
    let node = PhysicalPlanNode::decode(serialized_plan)
        .map_err(|e| DataFusionError::External(Box::new(e)))
        .map_err(py_datafusion_err)?;
    let plan = node
        .try_into_physical_plan(ctx, ctx.runtime_env().as_ref(), codec())
        .map_err(py_datafusion_err)?;
    Ok(plan)
}

/// Print DataFrame
fn print_dataframe(py: Python, df: DataFrame) -> PyDataFusionResult<()> {
    // Get string representation of record batches
    let batches = wait_for_future(py, df.collect())?;
    let batches_as_string = pretty::pretty_format_batches(&batches);
    let result = match batches_as_string {
        Ok(batch) => format!("DataFrame()\n{batch}"),
        Err(err) => format!("Error: {:?}", err.to_string()),
    };

    // Import the Python 'builtins' module to access the print function
    // Note that println! does not print to the Python debug console and is not visible in notebooks for instance
    let print = py.import("builtins")?.getattr("print")?;
    print.call1((result,))?;
    Ok(())
}

fn project_schema(from_schema: Schema, to_schema: Schema) -> Result<Schema, ArrowError> {
    let merged_schema = Schema::try_merge(vec![from_schema, to_schema.clone()])?;

    let project_indices: Vec<usize> = to_schema
        .fields
        .iter()
        .map(|field| field.name())
        .filter_map(|field_name| merged_schema.index_of(field_name).ok())
        .collect();

    merged_schema.project(&project_indices)
}

fn record_batch_into_schema(
    record_batch: RecordBatch,
    schema: &Schema,
) -> Result<RecordBatch, ArrowError> {
    let schema = Arc::new(schema.clone());
    let base_schema = record_batch.schema();
    if base_schema.fields().len() == 0 {
        // Nothing to project
        return Ok(RecordBatch::new_empty(schema));
    }

    let array_size = record_batch.column(0).len();
    let mut data_arrays = Vec::with_capacity(schema.fields().len());

    for field in schema.fields() {
        let desired_data_type = field.data_type();
        if let Some(original_data) = record_batch.column_by_name(field.name()) {
            let original_data_type = original_data.data_type();

            if can_cast_types(original_data_type, desired_data_type) {
                data_arrays.push(arrow::compute::kernels::cast(
                    original_data,
                    desired_data_type,
                )?);
            } else if field.is_nullable() {
                data_arrays.push(new_null_array(desired_data_type, array_size));
            } else {
                return Err(ArrowError::CastError(format!("Attempting to cast to non-nullable and non-castable field {} during schema projection.", field.name())));
            }
        } else {
            if !field.is_nullable() {
                return Err(ArrowError::CastError(format!(
                    "Attempting to set null to non-nullable field {} during schema projection.",
                    field.name()
                )));
            }
            data_arrays.push(new_null_array(desired_data_type, array_size));
        }
    }

    RecordBatch::try_new(schema, data_arrays)
}
