//! module for query executor
use std::any::Any;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use datafusion::catalog::CatalogProvider;
use datafusion::catalog::schema::SchemaProvider;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::prelude::Expr;
use datafusion_util::config::DEFAULT_SCHEMA;
use datafusion::common::Statistics;
use datafusion::execution::context::SessionState;
use datafusion::physical_plan::ExecutionPlan;
use data_types::{ChunkId, ChunkOrder, TransitionPartitionId};
use iox_query::exec::{Executor, ExecutorType, IOxSessionContext};
use iox_query::{QueryChunk, QueryChunkData, QueryCompletedToken, QueryNamespace, QueryText};
use iox_query::provider::ProviderBuilder;
use metric::Registry;
use observability_deps::tracing::info;
use schema::Schema;
use schema::sort::SortKey;
use service_common::planner::Planner;
use service_common::QueryNamespaceProvider;
use trace::ctx::SpanContext;
use trace::span::{Span, SpanExt, SpanRecorder};
use trace_http::ctx::RequestLogContext;
use tracker::{AsyncSemaphoreMetrics, InstrumentedAsyncOwnedSemaphorePermit, InstrumentedAsyncSemaphore};
use crate::{QueryExecutor, WriteBuffer};
use crate::catalog::{Catalog, DatabaseSchema};

#[derive(Debug)]
pub struct QueryExecutorImpl<W> {
    catalog: Arc<Catalog>,
    write_buffer: Arc<W>,
    exec: Arc<Executor>,
    datafusion_config: Arc<HashMap<String, String>>,
    query_execution_semaphore: Arc<InstrumentedAsyncSemaphore>,
}

impl<W: WriteBuffer> QueryExecutorImpl<W> {
    pub fn new(catalog: Arc<Catalog>, write_buffer: Arc<W>, exec: Arc<Executor>, metrics: Arc<Registry>, datafusion_config: Arc<HashMap<String, String>>, concurrent_query_limit: usize) -> Self {
        let semaphore_metrics = Arc::new(AsyncSemaphoreMetrics::new(&metrics, &[("semaphore", "query_execution")]));
        let query_execution_semaphore = Arc::new(semaphore_metrics.new_semaphore(concurrent_query_limit));
        Self {
            catalog,
            write_buffer,
            exec,
            datafusion_config,
            query_execution_semaphore,
        }
    }
}

#[async_trait]
impl<W: WriteBuffer> QueryExecutor for QueryExecutorImpl<W> {
    async fn query(&self, database: &str, q: &str, span_ctx: Option<SpanContext>, external_span_ctx: Option<RequestLogContext>) -> crate::Result<SendableRecordBatchStream> {
        info!("query in executor {}", database);
        let db = self.db(database, span_ctx.child_span("get database"), false).await.ok_or_else(|| {
            crate::Error::DatabaseNotFound {
                db_name: database.to_string(),
            }
        })?;

        let ctx = db.new_query_context(span_ctx);
        let _token = db.record_query(
            external_span_ctx.as_ref().map(RequestLogContext::ctx),
            "sql",
            Box::new(q.to_string()),
        );
        info!("plan");
        let plan = Planner::new(&ctx)
            .sql(q)
            .await?;

        info!("execute_stream");
        let query_results = ctx
            .execute_stream(Arc::clone(&plan))
            .await?;

        Ok(query_results)
    }
}

// This implementation is for the Flight service
#[async_trait]
impl<W: WriteBuffer> QueryNamespaceProvider for QueryExecutorImpl<W> {
    type Db = QueryDatabase;

    async fn db(&self, name: &str, span: Option<Span>, _include_debug_info_tables: bool) -> Option<Arc<Self::Db>> {
        let _span_recorder = SpanRecorder::new(span);

        let db_schema = self.catalog.db_schema(name)?;

        Some(Arc::new(QueryDatabase{
            db_schema,
            write_buffer: Arc::clone(&self.write_buffer) as _,
            exec: Arc::clone(&self.exec),
            datafusion_config: Arc::clone(&self.datafusion_config),
        }))
    }

    async fn acquire_semaphore(&self, span: Option<Span>) -> InstrumentedAsyncOwnedSemaphorePermit {
        Arc::clone(&self.query_execution_semaphore)
            .acquire_owned(span)
            .await
            .expect("Semaphore should not be closed by anyone")
    }
}

#[derive(Debug, Clone)]
pub struct QueryDatabase {
    db_schema: Arc<DatabaseSchema>,
    write_buffer: Arc<dyn WriteBuffer>,
    exec: Arc<Executor>,
    datafusion_config: Arc<HashMap<String, String>>,
}

impl QueryDatabase {
    pub fn new(db_schema: Arc<DatabaseSchema>, write_buffer: Arc<dyn WriteBuffer>, exec: Arc<Executor>, datafusion_config: Arc<HashMap<String, String>>) -> Self {
        Self {
            db_schema,
            write_buffer,
            exec,
            datafusion_config,
        }
    }
}

#[async_trait]
impl QueryNamespace for QueryDatabase {
    async fn chunks(&self, _table_name: &str, _filters: &[Expr], _projection: Option<&Vec<usize>>, _ctx: IOxSessionContext) -> Result<Vec<Arc<dyn QueryChunk>>, DataFusionError> {
        info!("called chunks on querydatabase");
        todo!()
    }

    fn retention_time_ns(&self) -> Option<i64> {
        None
    }

    fn record_query(&self, span_ctx: Option<&SpanContext>, query_type: &'static str, query_text: QueryText) -> QueryCompletedToken {
        let trace_id = span_ctx.map(|ctx| ctx.trace_id);
        QueryCompletedToken::new(move |success| {
            info!(?trace_id, %query_type, %query_text, %success, "query completed");
        })
    }

    fn new_query_context(&self, span_ctx: Option<SpanContext>) -> IOxSessionContext {
        let mut cfg = self
            .exec
            .new_execution_config(ExecutorType::Query)
            .with_default_catalog(Arc::new(self.clone()))
            .with_span_context(span_ctx);

        for (k, v) in self.datafusion_config.as_ref() {
            cfg = cfg.with_config_option(k, v);
        }

        cfg.build()
    }
}

impl CatalogProvider for QueryDatabase {
    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }

    fn schema_names(&self) -> Vec<String> {
        info!("CatalogProvider schema_names");
        vec![DEFAULT_SCHEMA.to_string()]
    }

    fn schema(&self, name: &str) -> Option<Arc<dyn SchemaProvider>> {
        info!("CatalogProvider schema {}", name);
        match name {
            DEFAULT_SCHEMA => Some(Arc::new(self.clone())),
            _ => None,
        }
    }
}

#[async_trait]
impl SchemaProvider for QueryDatabase {
    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }

    fn table_names(&self) -> Vec<String> {
        info!("table names");
        let mut names: Vec<_> = self.db_schema.tables.keys().cloned().collect();
        names.sort();
        names
    }

    async fn table(&self, name: &str) -> Option<Arc<dyn TableProvider>> {
        info!("table {}", name);

        let schema = self.db_schema.get_table_schema(name).unwrap();

        info!("return QueryTable");
        let name: Arc<str> = name.into();
        Some(Arc::new(QueryTable {
            db_schema: Arc::clone(&self.db_schema),
            name,
            schema,
            write_buffer: Arc::clone(&self.write_buffer),
        }))
    }

    fn table_exist(&self, name: &str) -> bool {
        info!("table exist {}", name);
        self.db_schema.tables.contains_key(name)
    }
}

#[derive(Debug)]
pub struct QueryTable {
    db_schema: Arc<DatabaseSchema>,
    name: Arc<str>,
    schema: Schema,
    write_buffer: Arc<dyn WriteBuffer>,
}

impl QueryTable {
    fn chunks(&self, ctx: &SessionState, projection: Option<&Vec<usize>>, filters: &[Expr], _limit: Option<usize>) -> Result<Vec<Arc<dyn QueryChunk>>, DataFusionError> {
        self.write_buffer.get_table_chunks(&self.db_schema.name, self.name.as_ref(), filters, projection, ctx)
    }
}

#[async_trait]
impl TableProvider for QueryTable {
    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }

    fn schema(&self) -> SchemaRef {
        self.schema.as_arrow()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(&self, ctx: &SessionState, projection: Option<&Vec<usize>>, filters: &[Expr], limit: Option<usize>) -> service_common::planner::Result<Arc<dyn ExecutionPlan>> {
        let filters = filters.to_vec();
        info!("TableProvider scan {:?} {:?} {:?}", projection, filters, limit);
        let mut builder =
            ProviderBuilder::new(Arc::clone(&self.name), self.schema.clone());

        let chunks = self.chunks(ctx, projection, &filters, limit)?;
        for chunk in chunks {
            builder = builder.add_chunk(chunk);
        }

        let provider = match builder.build() {
            Ok(provider) => provider,
            Err(e) => panic!("unexpected error: {e:?}"),
        };

        provider.scan(ctx, projection, &filters, limit).await
    }
}

#[derive(Debug)]
pub struct ParquetChunk {

}

impl QueryChunk for ParquetChunk {
    fn stats(&self) -> Arc<Statistics> {
        todo!()
    }

    fn schema(&self) -> &Schema {
        todo!()
    }

    fn partition_id(&self) -> &TransitionPartitionId {
        todo!()
    }

    fn sort_key(&self) -> Option<&SortKey> {
        todo!()
    }

    fn id(&self) -> ChunkId {
        todo!()
    }

    fn may_contain_pk_duplicates(&self) -> bool {
        todo!()
    }

    fn data(&self) -> QueryChunkData {
        todo!()
    }

    fn chunk_type(&self) -> &str {
        todo!()
    }

    fn order(&self) -> ChunkOrder {
        todo!()
    }

    fn as_any(&self) -> &dyn Any {
        todo!()
    }
}
