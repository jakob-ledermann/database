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

use crate::catalog_manager::CatalogManager;
use kernel::{SystemError, SystemResult};
use protocol::{
    results::{Description, QueryError, QueryEvent},
    Sender,
};
use sqlparser::ast::{Expr, Ident, Query, Select, SelectItem, SetExpr, TableFactor, TableWithJoins};
use std::{ops::Deref, sync::Arc};

pub(crate) struct SelectCommand<'sc> {
    raw_sql_query: &'sc str,
    query: Box<Query>,
    storage: Arc<CatalogManager>,
    session: Arc<dyn Sender>,
}

impl<'sc> SelectCommand<'sc> {
    pub(crate) fn new(
        raw_sql_query: &'sc str,
        query: Box<Query>,
        storage: Arc<CatalogManager>,
        session: Arc<dyn Sender>,
    ) -> SelectCommand<'sc> {
        SelectCommand {
            raw_sql_query,
            query,
            storage,
            session,
        }
    }

    pub(crate) fn describe(&mut self) -> SystemResult<Description> {
        let input = self.parse_select_input()?;

        let all_columns = self.storage.table_columns(&input.schema_name, &input.table_name)?;
        let mut column_definitions = vec![];
        let mut non_existing_columns = vec![];
        for column_name in &input.selected_columns {
            let mut found = None;
            for column_definition in &all_columns {
                if column_definition.has_name(&column_name) {
                    found = Some(column_definition);
                    break;
                }
            }

            if let Some(column_definition) = found {
                column_definitions.push(column_definition);
            } else {
                non_existing_columns.push(column_name.clone());
            }
        }

        if !non_existing_columns.is_empty() {
            self.session
                .send(Err(QueryError::column_does_not_exist(non_existing_columns)))
                .expect("To Send Result to Client");
            return Err(SystemError::runtime_check_failure("Column Does Not Exist".to_owned()));
        }

        let description = column_definitions
            .into_iter()
            .map(|column_definition| (column_definition.name(), (&column_definition.sql_type()).into()))
            .collect();

        Ok(description)
    }

    pub(crate) fn execute(&mut self) -> SystemResult<()> {
        let input = match self.parse_select_input() {
            Ok(input) => input,
            Err(_) => return Ok(()),
        };

        match self.storage.full_scan(&input.schema_name, &input.table_name) {
            Err(error) => return Err(error),
            Ok(records) => {
                let all_columns = self.storage.table_columns(&input.schema_name, &input.table_name)?;
                let mut description = vec![];
                let mut column_indexes = vec![];
                let mut non_existing_columns = vec![];
                for column_name in input.selected_columns.iter() {
                    let mut found = None;
                    for (index, column_definition) in all_columns.iter().enumerate() {
                        if column_definition.has_name(column_name) {
                            found = Some((index, column_definition.clone()));
                            break;
                        }
                    }

                    if let Some((index, column_definition)) = found {
                        column_indexes.push(index);
                        description.push(column_definition);
                    } else {
                        non_existing_columns.push(column_name.clone());
                    }
                }

                if !non_existing_columns.is_empty() {
                    self.session
                        .send(Err(QueryError::column_does_not_exist(non_existing_columns)))
                        .expect("To Send Result to Client");
                    return Ok(());
                }

                let values: Vec<Vec<String>> = records
                    .map(Result::unwrap)
                    .map(Result::unwrap)
                    .map(|(_key, values)| {
                        let row: Vec<String> = values.unpack().into_iter().map(|datum| datum.to_string()).collect();

                        let mut values = vec![];
                        for origin in column_indexes.iter() {
                            for (index, value) in row.iter().enumerate() {
                                if index == *origin {
                                    values.push(value.clone())
                                }
                            }
                        }
                        log::debug!("{:#?}", values);
                        values
                    })
                    .collect();

                let projection = (
                    description
                        .into_iter()
                        .map(|column| (column.name(), (&column.sql_type()).into()))
                        .collect(),
                    values,
                );
                self.session
                    .send(Ok(QueryEvent::RecordsSelected(projection)))
                    .expect("To Send Query Result to Client");
            }
        }

        Ok(())
    }

    fn parse_select_input(&self) -> SystemResult<SelectInput> {
        let Query { body, .. } = &*self.query;
        if let SetExpr::Select(select) = body {
            let Select { projection, from, .. } = select.deref();
            let TableWithJoins { relation, .. } = &from[0];
            let (schema_name, table_name) = match relation {
                TableFactor::Table { name, .. } => {
                    let table_name = name.0[1].to_string();
                    let schema_name = name.0[0].to_string();
                    (schema_name, table_name)
                }
                _ => {
                    self.session
                        .send(Err(QueryError::feature_not_supported(self.raw_sql_query.to_owned())))
                        .expect("To Send Query Result to Client");
                    return Err(SystemError::runtime_check_failure("Feature Not Supported".to_owned()));
                }
            };

            match self.storage.table_exists(&schema_name, &table_name) {
                None => {
                    self.session
                        .send(Err(QueryError::schema_does_not_exist(schema_name)))
                        .expect("To Send Result to Client");
                    Err(SystemError::runtime_check_failure("Schema Does Not Exist".to_owned()))
                }
                Some((_, None)) => {
                    self.session
                        .send(Err(QueryError::table_does_not_exist(
                            schema_name + "." + table_name.as_str(),
                        )))
                        .expect("To Send Result to Client");
                    Err(SystemError::runtime_check_failure("Table Does Not Exist".to_owned()))
                }
                Some((_, Some(_))) => {
                    let selected_columns = {
                        let projection = projection.clone();
                        let mut columns: Vec<String> = vec![];
                        for item in projection {
                            match item {
                                SelectItem::Wildcard => {
                                    let all_columns = self.storage.table_columns(&schema_name, &table_name)?;
                                    columns.extend(
                                        all_columns
                                            .into_iter()
                                            .map(|column_definition| column_definition.name())
                                            .collect::<Vec<String>>(),
                                    )
                                }
                                SelectItem::UnnamedExpr(Expr::Identifier(Ident { value, .. })) => {
                                    columns.push(value.clone())
                                }
                                _ => {
                                    self.session
                                        .send(Err(QueryError::feature_not_supported(self.raw_sql_query.to_owned())))
                                        .expect("To Send Query Result to Client");
                                    return Err(SystemError::runtime_check_failure("Feature Not Supported".to_owned()));
                                }
                            }
                        }
                        columns
                    };

                    Ok(SelectInput {
                        schema_name,
                        table_name,
                        selected_columns,
                    })
                }
            }
        } else {
            self.session
                .send(Err(QueryError::feature_not_supported(self.raw_sql_query.to_owned())))
                .expect("To Send Query Result to Client");
            Err(SystemError::runtime_check_failure("Feature Not Supported".to_owned()))
        }
    }
}

struct SelectInput {
    schema_name: String,
    table_name: String,
    selected_columns: Vec<String>,
}
