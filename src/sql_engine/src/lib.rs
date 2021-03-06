// Copyright 2020 Alex Dukhno
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

extern crate bigdecimal;
extern crate log;

use crate::{
    catalog_manager::CatalogManager,
    ddl::{
        create_schema::CreateSchemaCommand, create_table::CreateTableCommand, drop_schema::DropSchemaCommand,
        drop_table::DropTableCommand,
    },
    dml::{delete::DeleteCommand, insert::InsertCommand, select::SelectCommand, update::UpdateCommand},
    query::{bind::ParamBinder, plan::Plan, process::QueryProcessor},
    session::{statement::PreparedStatement, Session},
};
use itertools::izip;
use kernel::SystemResult;
use protocol::{
    results::{QueryError, QueryEvent},
    sql_formats::PostgreSqlFormat,
    sql_types::PostgreSqlType,
    sql_values::PostgreSqlValue,
    Sender,
};
use serde::{Deserialize, Serialize};
use sql_types::SqlType;
use sqlparser::{
    ast::Statement,
    dialect::{Dialect, PostgreSqlDialect},
    parser::Parser,
};
use std::{iter, sync::Arc};

pub mod catalog_manager;
mod ddl;
mod dml;
mod query;
mod session;

pub type Projection = (Vec<ColumnDefinition>, Vec<Vec<String>>);

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ColumnDefinition {
    name: String,
    sql_type: SqlType,
}

impl ColumnDefinition {
    pub fn new(name: &str, sql_type: SqlType) -> Self {
        Self {
            name: name.to_string(),
            sql_type,
        }
    }

    pub fn sql_type(&self) -> SqlType {
        self.sql_type
    }

    pub fn has_name(&self, other_name: &str) -> bool {
        self.name == other_name
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }
}

pub struct QueryExecutor {
    storage: Arc<CatalogManager>,
    sender: Arc<dyn Sender>,
    session: Session,
    processor: QueryProcessor,
    param_binder: ParamBinder,
}

impl QueryExecutor {
    pub fn new(storage: Arc<CatalogManager>, sender: Arc<dyn Sender>) -> Self {
        Self {
            storage: storage.clone(),
            sender: sender.clone(),
            session: Session::new(),
            processor: QueryProcessor::new(storage, sender.clone()),
            param_binder: ParamBinder::new(sender),
        }
    }

    #[allow(clippy::match_wild_err_arm)]
    pub fn execute(&mut self, raw_sql_query: &str) -> SystemResult<()> {
        let statement = match Parser::parse_sql(&PostgreSqlDialect {}, raw_sql_query) {
            Ok(mut statements) => {
                log::info!("stmts: {:#?}", statements);
                statements.pop().unwrap()
            }
            Err(e) => {
                log::error!("{:?} can't be parsed. Error: {:?}", raw_sql_query, e);
                self.sender
                    .send(Err(QueryError::syntax_error(format!(
                        "{:?} can't be parsed",
                        raw_sql_query
                    ))))
                    .expect("To Send Query Result to Client");
                return Ok(());
            }
        };

        self.process_statement(raw_sql_query, statement)?;

        self.sender
            .send(Ok(QueryEvent::QueryComplete))
            .expect("To Send Query Complete Event to Client");

        Ok(())
    }

    pub fn parse_prepared_statement(
        &mut self,
        statement_name: &str,
        raw_sql_query: &str,
        param_types: &[PostgreSqlType],
    ) -> SystemResult<()> {
        let statement = match Parser::parse_sql(&PreparedStatementDialect {}, raw_sql_query) {
            Ok(mut statements) => {
                log::info!("stmts: {:#?}", statements);
                statements.pop().unwrap()
            }
            Err(e) => {
                log::error!("{:?} can't be parsed. Error: {:?}", raw_sql_query, e);
                self.sender
                    .send(Err(QueryError::syntax_error(format!(
                        "{:?} can't be parsed",
                        raw_sql_query
                    ))))
                    .expect("To Send Query Result to Client");
                return Ok(());
            }
        };

        let description = match &statement {
            Statement::Query(query) => {
                SelectCommand::new(raw_sql_query, query.clone(), self.storage.clone(), self.sender.clone())
                    .describe()?
            }
            _ => vec![],
        };

        let prepared_statement = PreparedStatement::new(statement, param_types.to_vec(), description);
        self.session
            .set_prepared_statement(statement_name.to_owned(), prepared_statement);

        self.sender
            .send(Ok(QueryEvent::ParseComplete))
            .expect("To Send ParseComplete Event");

        Ok(())
    }

    pub fn describe_prepared_statement(&mut self, name: &str) -> SystemResult<()> {
        match self.session.get_prepared_statement(name) {
            Some(stmt) => {
                self.sender
                    .send(Ok(QueryEvent::PreparedStatementDescribed(
                        stmt.param_types().to_vec(),
                        stmt.description().to_vec(),
                    )))
                    .expect("To Send ParametersDescribed Event");
            }
            None => {
                self.sender
                    .send(Err(QueryError::prepared_statement_does_not_exist(name.to_owned())))
                    .expect("To Send Error to Client");
            }
        };

        Ok(())
    }

    pub fn bind_prepared_statement_to_portal(
        &mut self,
        portal_name: &str,
        statement_name: &str,
        param_formats: &[PostgreSqlFormat],
        raw_params: &[Option<Vec<u8>>],
        result_formats: &[PostgreSqlFormat],
    ) -> SystemResult<()> {
        let prepared_statement = match self.session.get_prepared_statement(statement_name) {
            Some(prepared_statement) => prepared_statement,
            None => {
                self.sender
                    .send(Err(QueryError::prepared_statement_does_not_exist(
                        statement_name.to_owned(),
                    )))
                    .expect("To Send Error to Client");
                return Ok(());
            }
        };

        let param_types = prepared_statement.param_types();
        if param_types.len() != raw_params.len() {
            let message = format!(
                "Bind message supplies {actual} parameters, \
                 but prepared statement \"{name}\" requires {expected}",
                name = statement_name,
                actual = raw_params.len(),
                expected = param_types.len()
            );
            self.sender
                .send(Err(QueryError::protocol_violation(message)))
                .expect("To Send Error to Client");
            return Ok(());
        }

        let param_formats = match pad_formats(param_formats, raw_params.len()) {
            Ok(param_formats) => param_formats,
            Err(msg) => {
                self.sender
                    .send(Err(QueryError::protocol_violation(msg)))
                    .expect("To Send Error to Client");
                return Ok(());
            }
        };

        let mut params: Vec<PostgreSqlValue> = vec![];
        for (raw_param, typ, format) in izip!(raw_params, param_types, param_formats) {
            match raw_param {
                None => params.push(PostgreSqlValue::Null),
                Some(bytes) => match typ.decode(&format, &bytes) {
                    Ok(param) => params.push(param),
                    Err(msg) => {
                        self.sender
                            .send(Err(QueryError::invalid_parameter_value(msg)))
                            .expect("To Send Error to Client");
                        return Ok(());
                    }
                },
            }
        }

        let mut new_stmt = prepared_statement.stmt().clone();
        if self.param_binder.bind(&mut new_stmt, &params).is_err() {
            return Ok(());
        }

        let result_formats = match pad_formats(result_formats, prepared_statement.description().len()) {
            Ok(result_formats) => result_formats,
            Err(msg) => {
                self.sender
                    .send(Err(QueryError::protocol_violation(msg)))
                    .expect("To Send Error to Client");
                return Ok(());
            }
        };

        self.session.set_portal(
            portal_name.to_owned(),
            statement_name.to_owned(),
            new_stmt,
            result_formats,
        );

        self.sender
            .send(Ok(QueryEvent::BindComplete))
            .expect("To Send BindComplete Event");

        Ok(())
    }

    // TODO: Parameter `max_rows` should be handled.
    pub fn execute_portal(&mut self, portal_name: &str, _max_rows: i32) -> SystemResult<()> {
        let portal = match self.session.get_portal(portal_name) {
            Some(portal) => portal,
            None => {
                self.sender
                    .send(Err(QueryError::portal_does_not_exist(portal_name.to_owned())))
                    .expect("To Send Error to Client");
                return Ok(());
            }
        };

        let statement = portal.stmt();
        let raw_sql_query = format!("{}", statement);
        self.process_statement(&raw_sql_query, statement.clone())
    }

    pub fn flush(&self) {
        match self.sender.flush() {
            Ok(_) => {}
            Err(e) => {
                log::error!("Flush error: {:?}", e);
            }
        };
    }

    fn process_statement(&self, raw_sql_query: &str, statement: Statement) -> SystemResult<()> {
        log::debug!("STATEMENT = {:?}", statement);
        match self.processor.process(statement) {
            Ok(Plan::CreateSchema(creation_info)) => {
                CreateSchemaCommand::new(creation_info, self.storage.clone(), self.sender.clone()).execute()?;
            }
            Ok(Plan::CreateTable(creation_info)) => {
                CreateTableCommand::new(creation_info, self.storage.clone(), self.sender.clone()).execute()?;
            }
            Ok(Plan::DropSchemas(schemas)) => {
                for (schema, cascade) in schemas {
                    DropSchemaCommand::new(schema, cascade, self.storage.clone(), self.sender.clone()).execute()?;
                }
            }
            Ok(Plan::DropTables(tables)) => {
                for table in tables {
                    DropTableCommand::new(table, self.storage.clone(), self.sender.clone()).execute()?;
                }
            }
            Ok(Plan::Insert(table_insert)) => {
                InsertCommand::new(raw_sql_query, table_insert, self.storage.clone(), self.sender.clone()).execute()?;
            }
            Ok(Plan::NotProcessed(statement)) => match *statement {
                Statement::StartTransaction { .. } => {
                    self.sender
                        .send(Ok(QueryEvent::TransactionStarted))
                        .expect("To Send Query Result to Client");
                }
                Statement::SetVariable { .. } => {
                    self.sender
                        .send(Ok(QueryEvent::VariableSet))
                        .expect("To Send Query Result to Client");
                }
                Statement::Drop { .. } => {
                    self.sender
                        .send(Err(QueryError::feature_not_supported(raw_sql_query.to_owned())))
                        .expect("To Send Query Result to Client");
                }
                Statement::Query(query) => {
                    SelectCommand::new(raw_sql_query, query, self.storage.clone(), self.sender.clone()).execute()?;
                }
                Statement::Update {
                    table_name,
                    assignments,
                    ..
                } => {
                    UpdateCommand::new(table_name, assignments, self.storage.clone(), self.sender.clone()).execute()?;
                }
                Statement::Delete { table_name, .. } => {
                    DeleteCommand::new(table_name, self.storage.clone(), self.sender.clone()).execute()?;
                }
                _ => {
                    self.sender
                        .send(Err(QueryError::feature_not_supported(raw_sql_query.to_owned())))
                        .expect("To Send Query Result to Client");
                }
            },
            Err(()) => {}
        };

        Ok(())
    }
}

#[derive(Debug)]
struct PreparedStatementDialect {}

impl Dialect for PreparedStatementDialect {
    fn is_identifier_start(&self, ch: char) -> bool {
        (ch >= 'a' && ch <= 'z') || (ch >= 'A' && ch <= 'Z') || ch == '$' || ch == '_'
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        (ch >= 'a' && ch <= 'z') || (ch >= 'A' && ch <= 'Z') || (ch >= '0' && ch <= '9') || ch == '$' || ch == '_'
    }
}

fn pad_formats(formats: &[PostgreSqlFormat], param_len: usize) -> Result<Vec<PostgreSqlFormat>, String> {
    match (formats.len(), param_len) {
        (0, n) => Ok(vec![PostgreSqlFormat::Text; n]),
        (1, n) => Ok(iter::repeat(formats[0]).take(n).collect()),
        (m, n) if m == n => Ok(formats.to_vec()),
        (m, n) => Err(format!("expected {} field format specifiers, but got {}", m, n)),
    }
}

#[cfg(test)]
mod tests;
