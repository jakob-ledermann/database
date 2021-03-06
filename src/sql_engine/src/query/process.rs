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

///! Module for transforming the input Query AST into representation the engine can process.
use crate::query::plan::{Plan, SchemaCreationInfo, TableCreationInfo, TableInserts};
use crate::{
    catalog_manager::CatalogManager,
    query::{SchemaId, SchemaNamingError, TableId, TableNamingError},
    ColumnDefinition,
};
use protocol::{results::QueryError, Sender};
use sql_types::SqlType;
use sqlparser::ast::{ColumnDef, DataType, ObjectName, ObjectType, Statement};
use std::{convert::TryFrom, sync::Arc};

type Result<T> = std::result::Result<T, ()>;

pub(crate) struct QueryProcessor {
    storage: Arc<CatalogManager>,
    sender: Arc<dyn Sender>,
}

impl<'qp> QueryProcessor {
    pub fn new(storage: Arc<CatalogManager>, sender: Arc<dyn Sender>) -> Self {
        Self { storage, sender }
    }

    pub fn process(&self, stmt: Statement) -> Result<Plan> {
        match stmt {
            Statement::CreateTable { name, columns, .. } => self.handle_create_table(name, &columns),
            Statement::CreateSchema { schema_name, .. } => {
                let schema_id = match SchemaId::try_from(schema_name) {
                    Ok(schema_id) => schema_id,
                    Err(SchemaNamingError(message)) => {
                        self.sender
                            .send(Err(QueryError::syntax_error(message)))
                            .expect("To Send Query Result to Client");
                        return Err(());
                    }
                };
                match self.storage.schema_exists(schema_id.name()) {
                    Some(_) => {
                        self.sender
                            .send(Err(QueryError::schema_already_exists(schema_id.name().to_string())))
                            .expect("To Send Query Result to Client");
                        Err(())
                    }
                    None => Ok(Plan::CreateSchema(SchemaCreationInfo {
                        schema_name: schema_id.name().to_string(),
                    })),
                }
            }
            Statement::Drop {
                object_type,
                names,
                cascade,
                ..
            } => self.handle_drop(&object_type, &names, cascade),
            Statement::Insert {
                table_name,
                columns,
                source,
            } => match TableId::try_from(table_name) {
                Ok(table_id) => Ok(Plan::Insert(TableInserts {
                    table_id,
                    column_indices: columns,
                    input: source,
                })),
                Err(TableNamingError(message)) => {
                    self.sender
                        .send(Err(QueryError::syntax_error(message)))
                        .expect("To Send Query Result to Client");
                    Err(())
                }
            },
            _ => Ok(Plan::NotProcessed(Box::new(stmt.clone()))),
        }
    }

    fn sql_type_from_datatype(&self, datatype: &DataType) -> Result<SqlType> {
        match datatype {
            DataType::SmallInt => Ok(SqlType::SmallInt(i16::min_value())),
            DataType::Int => Ok(SqlType::Integer(i32::min_value())),
            DataType::BigInt => Ok(SqlType::BigInt(i64::min_value())),
            DataType::Char(len) => Ok(SqlType::Char(len.unwrap_or(255))),
            DataType::Varchar(len) => Ok(SqlType::VarChar(len.unwrap_or(255))),
            DataType::Boolean => Ok(SqlType::Bool),
            DataType::Custom(name) => {
                let name = name.to_string();
                match name.as_str() {
                    "serial" => Ok(SqlType::Integer(1)),
                    "smallserial" => Ok(SqlType::SmallInt(1)),
                    "bigserial" => Ok(SqlType::BigInt(1)),
                    other_type => {
                        self.sender
                            .send(Err(QueryError::feature_not_supported(format!(
                                "{} type is not supported",
                                other_type
                            ))))
                            .expect("To Send Query Result to Client");
                        Err(())
                    }
                }
            }
            other_type => {
                self.sender
                    .send(Err(QueryError::feature_not_supported(format!(
                        "{} type is not supported",
                        other_type
                    ))))
                    .expect("To Send Query Result to Client");
                Err(())
            }
        }
    }

    fn resolve_column_definitions(&self, columns: &[ColumnDef]) -> Result<Vec<ColumnDefinition>> {
        let mut column_defs = Vec::new();
        for column in columns {
            let sql_type = self.sql_type_from_datatype(&column.data_type)?;
            // maybe a different type should be used to represent this instead of the storage's representation.
            let column_definition = ColumnDefinition::new(column.name.value.as_str(), sql_type);
            column_defs.push(column_definition);
        }
        Ok(column_defs)
    }

    fn handle_create_table(&self, name: ObjectName, columns: &[ColumnDef]) -> Result<Plan> {
        let table_id = match TableId::try_from(name) {
            Ok(table_id) => table_id,
            Err(TableNamingError(message)) => {
                self.sender
                    .send(Err(QueryError::syntax_error(message)))
                    .expect("To Send Query Result to Client");
                return Err(());
            }
        };
        let schema_name = table_id.schema_name();
        let table_name = table_id.name();
        match self.storage.table_exists(&schema_name, &table_name) {
            None => {
                self.sender
                    .send(Err(QueryError::schema_does_not_exist(schema_name.to_owned())))
                    .expect("To Send Query Result to Client");
                Err(())
            }
            Some((_, Some(_))) => {
                self.sender
                    .send(Err(QueryError::table_already_exists(format!(
                        "{}.{}",
                        schema_name, table_name
                    ))))
                    .expect("To Send Query Result to Client");
                Err(())
            }
            Some((_, None)) => {
                let columns = self.resolve_column_definitions(columns)?;
                let table_info = TableCreationInfo {
                    schema_name: schema_name.to_owned(),
                    table_name: table_name.to_owned(),
                    columns,
                };
                Ok(Plan::CreateTable(table_info))
            }
        }
    }

    fn handle_drop(&self, object_type: &ObjectType, names: &[ObjectName], cascade: bool) -> Result<Plan> {
        match object_type {
            ObjectType::Table => {
                let mut table_names = Vec::with_capacity(names.len());
                for name in names {
                    // I like the idea of abstracting this to a resolve_table_name(name) which would do
                    // this check for us and can be reused else where. ideally this function could handle aliasing as well.
                    let table_id = match TableId::try_from(name.clone()) {
                        Ok(table_id) => table_id,
                        Err(TableNamingError(message)) => {
                            self.sender
                                .send(Err(QueryError::syntax_error(message)))
                                .expect("To Send Query Result to Client");
                            return Err(());
                        }
                    };
                    let schema_name = table_id.schema_name();
                    let table_name = table_id.name();
                    match self.storage.table_exists(&schema_name, &table_name) {
                        None => {
                            self.sender
                                .send(Err(QueryError::schema_does_not_exist(schema_name.to_owned())))
                                .expect("To Send Query Result to Client");
                            return Err(());
                        }
                        Some((_, None)) => {
                            self.sender
                                .send(Err(QueryError::table_does_not_exist(format!(
                                    "{}.{}",
                                    schema_name, table_name
                                ))))
                                .expect("To Send Query Result to Client");
                            return Err(());
                        }
                        Some((_, Some(_))) => table_names.push(table_id),
                    }
                }
                Ok(Plan::DropTables(table_names))
            }
            ObjectType::Schema => {
                let mut schema_names = Vec::with_capacity(names.len());
                for name in names {
                    let schema_id = match SchemaId::try_from(name.clone()) {
                        Ok(schema_id) => schema_id,
                        Err(SchemaNamingError(message)) => {
                            self.sender
                                .send(Err(QueryError::syntax_error(message)))
                                .expect("To Send Query Result to Client");
                            return Err(());
                        }
                    };
                    match self.storage.schema_exists(schema_id.name()) {
                        None => {
                            self.sender
                                .send(Err(QueryError::schema_does_not_exist(schema_id.name().to_owned())))
                                .expect("To Send Query Result to Client");
                            return Err(());
                        }
                        Some(_) => schema_names.push((schema_id, cascade)),
                    }
                }
                Ok(Plan::DropSchemas(schema_names))
            }
            _ => unimplemented!(),
        }
    }
}
