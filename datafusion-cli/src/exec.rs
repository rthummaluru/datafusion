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

//! Execution functions

use crate::cli_context::CliSessionContext;
use crate::helper::split_from_semicolon;
use crate::print_format::PrintFormat;
use crate::{
    command::{Command, OutputFormat},
    helper::CliHelper,
    object_storage::get_object_store,
    print_options::{MaxRows, PrintOptions},
};
use datafusion::common::instant::Instant;
use datafusion::common::{plan_datafusion_err, plan_err};
use datafusion::config::ConfigFileType;
use datafusion::datasource::listing::ListingTableUrl;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::memory_pool::MemoryConsumer;
use datafusion::logical_expr::{DdlStatement, LogicalPlan};
use datafusion::physical_plan::execution_plan::EmissionType;
use datafusion::physical_plan::spill::get_record_batch_memory_size;
use datafusion::physical_plan::{execute_stream, ExecutionPlanProperties};
use datafusion::sql::parser::{DFParser, Statement};
use datafusion::sql::sqlparser;
use datafusion::sql::sqlparser::dialect::dialect_from_str;
use futures::StreamExt;
use log::warn;
use object_store::Error::Generic;
use rustyline::error::ReadlineError;
use rustyline::Editor;
use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufReader;
use tokio::signal;

/// run and execute SQL statements and commands, against a context with the given print options
pub async fn exec_from_commands(
    ctx: &dyn CliSessionContext,
    commands: Vec<String>,
    print_options: &PrintOptions,
) -> Result<()> {
    for sql in commands {
        exec_and_print(ctx, print_options, sql).await?;
    }

    Ok(())
}

/// run and execute SQL statements and commands from a file, against a context with the given print options
pub async fn exec_from_lines(
    ctx: &dyn CliSessionContext,
    reader: &mut BufReader<File>,
    print_options: &PrintOptions,
) -> Result<()> {
    let mut query = "".to_owned();

    for line in reader.lines() {
        match line {
            Ok(line) if line.starts_with("#!") => {
                continue;
            }
            Ok(line) if line.starts_with("--") => {
                continue;
            }
            Ok(line) => {
                let line = line.trim_end();
                query.push_str(line);
                if line.ends_with(';') {
                    match exec_and_print(ctx, print_options, query).await {
                        Ok(_) => {}
                        Err(err) => eprintln!("{err}"),
                    }
                    query = "".to_string();
                } else {
                    query.push('\n');
                }
            }
            _ => {
                break;
            }
        }
    }

    // run the left over query if the last statement doesn't contain ‘;’
    // ignore if it only consists of '\n'
    if query.contains(|c| c != '\n') {
        exec_and_print(ctx, print_options, query).await?;
    }

    Ok(())
}

pub async fn exec_from_files(
    ctx: &dyn CliSessionContext,
    files: Vec<String>,
    print_options: &PrintOptions,
) -> Result<()> {
    let files = files
        .into_iter()
        .map(|file_path| File::open(file_path).unwrap())
        .collect::<Vec<_>>();

    for file in files {
        let mut reader = BufReader::new(file);
        exec_from_lines(ctx, &mut reader, print_options).await?;
    }

    Ok(())
}

/// run and execute SQL statements and commands against a context with the given print options
pub async fn exec_from_repl(
    ctx: &dyn CliSessionContext,
    print_options: &mut PrintOptions,
) -> rustyline::Result<()> {
    let mut rl = Editor::new()?;
    rl.set_helper(Some(CliHelper::new(
        &ctx.task_ctx().session_config().options().sql_parser.dialect,
        print_options.color,
    )));
    rl.load_history(".history").ok();

    loop {
        match rl.readline("> ") {
            Ok(line) if line.starts_with('\\') => {
                rl.add_history_entry(line.trim_end())?;
                let command = line.split_whitespace().collect::<Vec<_>>().join(" ");
                if let Ok(cmd) = &command[1..].parse::<Command>() {
                    match cmd {
                        Command::Quit => break,
                        Command::OutputFormat(subcommand) => {
                            if let Some(subcommand) = subcommand {
                                if let Ok(command) = subcommand.parse::<OutputFormat>() {
                                    if let Err(e) = command.execute(print_options).await {
                                        eprintln!("{e}")
                                    }
                                } else {
                                    eprintln!(
                                        "'\\{}' is not a valid command",
                                        &line[1..]
                                    );
                                }
                            } else {
                                println!("Output format is {:?}.", print_options.format);
                            }
                        }
                        _ => {
                            if let Err(e) = cmd.execute(ctx, print_options).await {
                                eprintln!("{e}")
                            }
                        }
                    }
                } else {
                    eprintln!("'\\{}' is not a valid command", &line[1..]);
                }
            }
            Ok(line) => {
                let lines = split_from_semicolon(&line);
                for line in lines {
                    rl.add_history_entry(line.trim_end())?;
                    tokio::select! {
                        res = exec_and_print(ctx, print_options, line) => match res {
                            Ok(_) => {}
                            Err(err) => eprintln!("{err}"),
                        },
                        _ = signal::ctrl_c() => {
                            println!("^C");
                            continue
                        },
                    }
                    // dialect might have changed
                    rl.helper_mut().unwrap().set_dialect(
                        &ctx.task_ctx().session_config().options().sql_parser.dialect,
                    );
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!("\\q");
                break;
            }
            Err(err) => {
                eprintln!("Unknown error happened {err:?}");
                break;
            }
        }
    }

    rl.save_history(".history")
}

pub(super) async fn exec_and_print(
    ctx: &dyn CliSessionContext,
    print_options: &PrintOptions,
    sql: String,
) -> Result<()> {
    let task_ctx = ctx.task_ctx();
    let options = task_ctx.session_config().options();
    let dialect = &options.sql_parser.dialect;
    let dialect = dialect_from_str(dialect).ok_or_else(|| {
        plan_datafusion_err!(
            "Unsupported SQL dialect: {dialect}. Available dialects: \
                 Generic, MySQL, PostgreSQL, Hive, SQLite, Snowflake, Redshift, \
                 MsSQL, ClickHouse, BigQuery, Ansi, DuckDB, Databricks."
        )
    })?;

    let statements = DFParser::parse_sql_with_dialect(&sql, dialect.as_ref())?;
    for statement in statements {
        StatementExecutor::new(statement)
            .execute(ctx, print_options)
            .await?;
    }

    Ok(())
}

/// Executor for SQL statements, including special handling for S3 region detection retry logic
struct StatementExecutor {
    statement: Statement,
    statement_for_retry: Option<Statement>,
}

impl StatementExecutor {
    fn new(statement: Statement) -> Self {
        let statement_for_retry = matches!(statement, Statement::CreateExternalTable(_))
            .then(|| statement.clone());

        Self {
            statement,
            statement_for_retry,
        }
    }

    async fn execute(
        self,
        ctx: &dyn CliSessionContext,
        print_options: &PrintOptions,
    ) -> Result<()> {
        let now = Instant::now();
        let (df, adjusted) = self
            .create_and_execute_logical_plan(ctx, print_options)
            .await?;
        let physical_plan = df.create_physical_plan().await?;
        let task_ctx = ctx.task_ctx();
        let options = task_ctx.session_config().options();

        // Track memory usage for the query result if it's bounded
        let mut reservation =
            MemoryConsumer::new("DataFusion-Cli").register(task_ctx.memory_pool());

        if physical_plan.boundedness().is_unbounded() {
            if physical_plan.pipeline_behavior() == EmissionType::Final {
                return plan_err!(
                    "The given query can generate a valid result only once \
                    the source finishes, but the source is unbounded"
                );
            }
            // As the input stream comes, we can generate results.
            // However, memory safety is not guaranteed.
            let stream = execute_stream(physical_plan, task_ctx.clone())?;
            print_options
                .print_stream(stream, now, &options.format)
                .await?;
        } else {
            // Bounded stream; collected results size is limited by the maxrows option
            let schema = physical_plan.schema();
            let mut stream = execute_stream(physical_plan, task_ctx.clone())?;
            let mut results = vec![];
            let mut row_count = 0_usize;
            let max_rows = match print_options.maxrows {
                MaxRows::Unlimited => usize::MAX,
                MaxRows::Limited(n) => n,
            };
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                let curr_num_rows = batch.num_rows();
                // Stop collecting results if the number of rows exceeds the limit
                // results batch should include the last batch that exceeds the limit
                if row_count < max_rows + curr_num_rows {
                    // Try to grow the reservation to accommodate the batch in memory
                    reservation.try_grow(get_record_batch_memory_size(&batch))?;
                    results.push(batch);
                }
                row_count += curr_num_rows;
            }
            adjusted.into_inner().print_batches(
                schema,
                &results,
                now,
                row_count,
                &options.format,
            )?;
            reservation.free();
        }

        Ok(())
    }

    async fn create_and_execute_logical_plan(
        mut self,
        ctx: &dyn CliSessionContext,
        print_options: &PrintOptions,
    ) -> Result<(datafusion::dataframe::DataFrame, AdjustedPrintOptions)> {
        let adjusted = AdjustedPrintOptions::new(print_options.clone())
            .with_statement(&self.statement);

        let plan = create_plan(ctx, self.statement, false).await?;
        let adjusted = adjusted.with_plan(&plan);

        let df = match ctx.execute_logical_plan(plan).await {
            Ok(df) => Ok(df),
            Err(DataFusionError::ObjectStore(err))
                if matches!(err.as_ref(), Generic { store, source: _ } if "S3".eq_ignore_ascii_case(store))
                    && self.statement_for_retry.is_some() =>
            {
                warn!("S3 region is incorrect, auto-detecting the correct region (this may be slow). Consider updating your region configuration.");
                let plan =
                    create_plan(ctx, self.statement_for_retry.take().unwrap(), true)
                        .await?;
                ctx.execute_logical_plan(plan).await
            }
            Err(e) => Err(e),
        }?;

        Ok((df, adjusted))
    }
}

/// Track adjustments to the print options based on the plan / statement being executed
#[derive(Debug)]
struct AdjustedPrintOptions {
    inner: PrintOptions,
}

impl AdjustedPrintOptions {
    fn new(inner: PrintOptions) -> Self {
        Self { inner }
    }
    /// Adjust print options based on any statement specific requirements
    fn with_statement(mut self, statement: &Statement) -> Self {
        if let Statement::Statement(sql_stmt) = statement {
            // SHOW / SHOW ALL
            if let sqlparser::ast::Statement::ShowVariable { .. } = sql_stmt.as_ref() {
                self.inner.maxrows = MaxRows::Unlimited
            }
        }
        self
    }

    /// Adjust print options based on any plan specific requirements
    fn with_plan(mut self, plan: &LogicalPlan) -> Self {
        // For plans like `Explain` ignore `MaxRows` option and always display
        // all rows
        if matches!(
            plan,
            LogicalPlan::Explain(_)
                | LogicalPlan::DescribeTable(_)
                | LogicalPlan::Analyze(_)
        ) {
            self.inner.maxrows = MaxRows::Unlimited;
        }
        self
    }

    /// Finalize and return the inner `PrintOptions`
    fn into_inner(mut self) -> PrintOptions {
        if self.inner.format == PrintFormat::Automatic {
            self.inner.format = PrintFormat::Table;
        }

        self.inner
    }
}

fn config_file_type_from_str(ext: &str) -> Option<ConfigFileType> {
    match ext.to_lowercase().as_str() {
        "csv" => Some(ConfigFileType::CSV),
        "json" => Some(ConfigFileType::JSON),
        "parquet" => Some(ConfigFileType::PARQUET),
        _ => None,
    }
}

async fn create_plan(
    ctx: &dyn CliSessionContext,
    statement: Statement,
    resolve_region: bool,
) -> Result<LogicalPlan, DataFusionError> {
    let mut plan = ctx.session_state().statement_to_plan(statement).await?;

    // Note that cmd is a mutable reference so that create_external_table function can remove all
    // datafusion-cli specific options before passing through to datafusion. Otherwise, datafusion
    // will raise Configuration errors.
    if let LogicalPlan::Ddl(DdlStatement::CreateExternalTable(cmd)) = &plan {
        // To support custom formats, treat error as None
        let format = config_file_type_from_str(&cmd.file_type);
        register_object_store_and_config_extensions(
            ctx,
            &cmd.location,
            &cmd.options,
            format,
            resolve_region,
        )
        .await?;
    }

    if let LogicalPlan::Copy(copy_to) = &mut plan {
        let format = config_file_type_from_str(&copy_to.file_type.get_ext());

        register_object_store_and_config_extensions(
            ctx,
            &copy_to.output_url,
            &copy_to.options,
            format,
            false,
        )
        .await?;
    }
    Ok(plan)
}

/// Asynchronously registers an object store and its configuration extensions
/// to the session context.
///
/// This function dynamically registers a cloud object store based on the given
/// location and options. It first parses the location to determine the scheme
/// and constructs the URL accordingly. Depending on the scheme, it also registers
/// relevant options. The function then alters the default table options with the
/// given custom options. Finally, it retrieves and registers the object store
/// in the session context.
///
/// # Parameters
///
/// * `ctx`: A reference to the `SessionContext` for registering the object store.
/// * `location`: A string reference representing the location of the object store.
/// * `options`: A reference to a hash map containing configuration options for
///   the object store.
///
/// # Returns
///
/// A `Result<()>` which is an Ok value indicating successful registration, or
/// an error upon failure.
///
/// # Errors
///
/// This function can return an error if the location parsing fails, options
/// alteration fails, or if the object store cannot be retrieved and registered
/// successfully.
pub(crate) async fn register_object_store_and_config_extensions(
    ctx: &dyn CliSessionContext,
    location: &String,
    options: &HashMap<String, String>,
    format: Option<ConfigFileType>,
    resolve_region: bool,
) -> Result<()> {
    // Parse the location URL to extract the scheme and other components
    let table_path = ListingTableUrl::parse(location)?;

    // Extract the scheme (e.g., "s3", "gcs") from the parsed URL
    let scheme = table_path.scheme();

    // Obtain a reference to the URL
    let url = table_path.as_ref();

    // Register the options based on the scheme extracted from the location
    ctx.register_table_options_extension_from_scheme(scheme);

    // Clone and modify the default table options based on the provided options
    let mut table_options = ctx.session_state().default_table_options();
    if let Some(format) = format {
        table_options.set_config_format(format);
    }
    table_options.alter_with_string_hash_map(options)?;

    // Retrieve the appropriate object store based on the scheme, URL, and modified table options
    let store = get_object_store(
        &ctx.session_state(),
        scheme,
        url,
        &table_options,
        resolve_region,
    )
    .await?;

    // Register the retrieved object store in the session context's runtime environment
    ctx.register_object_store(url, store);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use datafusion::common::plan_err;

    use datafusion::prelude::SessionContext;
    use url::Url;

    async fn create_external_table_test(location: &str, sql: &str) -> Result<()> {
        let ctx = SessionContext::new();
        let plan = ctx.state().create_logical_plan(sql).await?;

        if let LogicalPlan::Ddl(DdlStatement::CreateExternalTable(cmd)) = &plan {
            let format = config_file_type_from_str(&cmd.file_type);
            register_object_store_and_config_extensions(
                &ctx,
                &cmd.location,
                &cmd.options,
                format,
                false,
            )
            .await?;
        } else {
            return plan_err!("LogicalPlan is not a CreateExternalTable");
        }

        // Ensure the URL is supported by the object store
        ctx.runtime_env()
            .object_store(ListingTableUrl::parse(location)?)?;

        Ok(())
    }

    async fn copy_to_table_test(location: &str, sql: &str) -> Result<()> {
        let ctx = SessionContext::new();
        // AWS CONFIG register.

        let plan = ctx.state().create_logical_plan(sql).await?;

        if let LogicalPlan::Copy(cmd) = &plan {
            let format = config_file_type_from_str(&cmd.file_type.get_ext());
            register_object_store_and_config_extensions(
                &ctx,
                &cmd.output_url,
                &cmd.options,
                format,
                false,
            )
            .await?;
        } else {
            return plan_err!("LogicalPlan is not a CreateExternalTable");
        }

        // Ensure the URL is supported by the object store
        ctx.runtime_env()
            .object_store(ListingTableUrl::parse(location)?)?;

        Ok(())
    }

    #[tokio::test]
    async fn create_object_store_table_http() -> Result<()> {
        // Should be OK
        let location = "http://example.com/file.parquet";
        let sql =
            format!("CREATE EXTERNAL TABLE test STORED AS PARQUET LOCATION '{location}'");
        create_external_table_test(location, &sql).await?;

        Ok(())
    }
    #[tokio::test]
    async fn copy_to_external_object_store_test() -> Result<()> {
        let locations = vec![
            "s3://bucket/path/file.parquet",
            "oss://bucket/path/file.parquet",
            "cos://bucket/path/file.parquet",
            "gcs://bucket/path/file.parquet",
        ];
        let ctx = SessionContext::new();
        let task_ctx = ctx.task_ctx();
        let dialect = &task_ctx.session_config().options().sql_parser.dialect;
        let dialect = dialect_from_str(dialect).ok_or_else(|| {
            plan_datafusion_err!(
                "Unsupported SQL dialect: {dialect}. Available dialects: \
                 Generic, MySQL, PostgreSQL, Hive, SQLite, Snowflake, Redshift, \
                 MsSQL, ClickHouse, BigQuery, Ansi, DuckDB, Databricks."
            )
        })?;
        for location in locations {
            let sql = format!("copy (values (1,2)) to '{location}' STORED AS PARQUET;");
            let statements = DFParser::parse_sql_with_dialect(&sql, dialect.as_ref())?;
            for statement in statements {
                //Should not fail
                let mut plan = create_plan(&ctx, statement, false).await?;
                if let LogicalPlan::Copy(copy_to) = &mut plan {
                    assert_eq!(copy_to.output_url, location);
                    assert_eq!(copy_to.file_type.get_ext(), "parquet".to_string());
                    ctx.runtime_env()
                        .object_store_registry
                        .get_store(&Url::parse(&copy_to.output_url).unwrap())?;
                } else {
                    return plan_err!("LogicalPlan is not a CopyTo");
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn copy_to_object_store_table_s3() -> Result<()> {
        let access_key_id = "fake_access_key_id";
        let secret_access_key = "fake_secret_access_key";
        let location = "s3://bucket/path/file.parquet";

        // Missing region, use object_store defaults
        let sql = format!("COPY (values (1,2)) TO '{location}' STORED AS PARQUET
            OPTIONS ('aws.access_key_id' '{access_key_id}', 'aws.secret_access_key' '{secret_access_key}')");
        copy_to_table_test(location, &sql).await?;

        Ok(())
    }

    #[tokio::test]
    async fn create_object_store_table_s3() -> Result<()> {
        let access_key_id = "fake_access_key_id";
        let secret_access_key = "fake_secret_access_key";
        let region = "fake_us-east-2";
        let session_token = "fake_session_token";
        let location = "s3://bucket/path/file.parquet";

        // Missing region, use object_store defaults
        let sql = format!("CREATE EXTERNAL TABLE test STORED AS PARQUET
            OPTIONS('aws.access_key_id' '{access_key_id}', 'aws.secret_access_key' '{secret_access_key}') LOCATION '{location}'");
        create_external_table_test(location, &sql).await?;

        // Should be OK
        let sql = format!("CREATE EXTERNAL TABLE test STORED AS PARQUET
            OPTIONS('aws.access_key_id' '{access_key_id}', 'aws.secret_access_key' '{secret_access_key}', 'aws.region' '{region}', 'aws.session_token' '{session_token}') LOCATION '{location}'");
        create_external_table_test(location, &sql).await?;

        Ok(())
    }

    #[tokio::test]
    async fn create_object_store_table_oss() -> Result<()> {
        let access_key_id = "fake_access_key_id";
        let secret_access_key = "fake_secret_access_key";
        let endpoint = "fake_endpoint";
        let location = "oss://bucket/path/file.parquet";

        // Should be OK
        let sql = format!("CREATE EXTERNAL TABLE test STORED AS PARQUET
            OPTIONS('aws.access_key_id' '{access_key_id}', 'aws.secret_access_key' '{secret_access_key}', 'aws.oss.endpoint' '{endpoint}') LOCATION '{location}'");
        create_external_table_test(location, &sql).await?;

        Ok(())
    }

    #[tokio::test]
    async fn create_object_store_table_cos() -> Result<()> {
        let access_key_id = "fake_access_key_id";
        let secret_access_key = "fake_secret_access_key";
        let endpoint = "fake_endpoint";
        let location = "cos://bucket/path/file.parquet";

        // Should be OK
        let sql = format!("CREATE EXTERNAL TABLE test STORED AS PARQUET
            OPTIONS('aws.access_key_id' '{access_key_id}', 'aws.secret_access_key' '{secret_access_key}', 'aws.cos.endpoint' '{endpoint}') LOCATION '{location}'");
        create_external_table_test(location, &sql).await?;

        Ok(())
    }

    #[tokio::test]
    async fn create_object_store_table_gcs() -> Result<()> {
        let service_account_path = "fake_service_account_path";
        let service_account_key =
            "{\"private_key\": \"fake_private_key.pem\",\"client_email\":\"fake_client_email\", \"private_key_id\":\"id\"}";
        let application_credentials_path = "fake_application_credentials_path";
        let location = "gcs://bucket/path/file.parquet";

        // for service_account_path
        let sql = format!("CREATE EXTERNAL TABLE test STORED AS PARQUET
            OPTIONS('gcp.service_account_path' '{service_account_path}') LOCATION '{location}'");
        let err = create_external_table_test(location, &sql)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("os error 2"));

        // for service_account_key
        let sql = format!("CREATE EXTERNAL TABLE test STORED AS PARQUET OPTIONS('gcp.service_account_key' '{service_account_key}') LOCATION '{location}'");
        let err = create_external_table_test(location, &sql)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("No RSA key found in pem file"), "{err}");

        // for application_credentials_path
        let sql = format!("CREATE EXTERNAL TABLE test STORED AS PARQUET
            OPTIONS('gcp.application_credentials_path' '{application_credentials_path}') LOCATION '{location}'");
        let err = create_external_table_test(location, &sql)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("os error 2"));

        Ok(())
    }

    #[tokio::test]
    async fn create_external_table_local_file() -> Result<()> {
        let location = "path/to/file.parquet";

        // Ensure that local files are also registered
        let sql =
            format!("CREATE EXTERNAL TABLE test STORED AS PARQUET LOCATION '{location}'");
        create_external_table_test(location, &sql).await.unwrap();

        Ok(())
    }

    #[tokio::test]
    async fn create_external_table_format_option() -> Result<()> {
        let location = "path/to/file.cvs";

        // Test with format options
        let sql =
            format!("CREATE EXTERNAL TABLE test STORED AS CSV LOCATION '{location}' OPTIONS('format.has_header' 'true')");
        create_external_table_test(location, &sql).await.unwrap();

        Ok(())
    }
}
