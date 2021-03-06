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

//! Prepared statements maintain in-progress state during a session.
//!
//! In PostgreSQL there are two ways to construct prepared statements:
//!
//! * Via an explicit, user-provided `PREPARE <name> AS <sql>` sql statement.
//! * As part of the PostgreSQL Frontend/Backend protocol, where prepared
//!   statements are created implicitly by client libraries on behalf of users.
//!
//! For Frontend/Backend protocol, there are multiple steps to use prepared
//! statements:
//!
//! 1. Receive a `Parse` message. `Parse` messages included a name for the
//!    prepared statement, in addition to some other possible metadata.
//! 2. After validation, we stash the statement in the `Session` associated with
//!    the current user's session.
//! 3. The client issues a `Bind` message, which provides a name for a portal,
//!    and associates that name with a previously-named prepared statement. This
//!    is the point at which all possible parameters are associated with the
//!    statement, there are no longer any free variables permited.
//! 4. The client issues an `Execute` message with the name of a portal, causing
//!    that portal to actually start scanning and returning results.

use protocol::{results::Description, sql_formats::PostgreSqlFormat, sql_types::PostgreSqlType};
use sqlparser::ast::Statement;

/// A prepared statement.
#[derive(Clone, Debug)]
pub struct PreparedStatement {
    /// The raw prepared SQL statement will be bound to a portal.
    stmt: Statement,
    /// The types of any bound parameters.
    param_types: Vec<PostgreSqlType>,
    /// The type of the rows that will be returned.
    description: Description,
}

impl PreparedStatement {
    /// Constructs a new `PreparedStatement`.
    pub fn new(stmt: Statement, param_types: Vec<PostgreSqlType>, description: Description) -> PreparedStatement {
        PreparedStatement {
            stmt,
            param_types,
            description,
        }
    }

    /// Returns the raw prepared SQL statement.
    pub fn stmt(&self) -> &Statement {
        &self.stmt
    }

    /// Returns the types of any bound parameters.
    pub fn param_types(&self) -> &[PostgreSqlType] {
        &self.param_types
    }

    /// Returns the type of the rows that will be returned.
    pub fn description(&self) -> &[(String, PostgreSqlType)] {
        self.description.as_ref()
    }
}

/// A portal represents the execution state of a running or runnable query.
#[derive(Clone, Debug)]
pub struct Portal {
    /// The name of the prepared statement that is bound to this portal.
    statement_name: String,
    /// The bound SQL statement from the prepared statement.
    stmt: Statement,
    /// The desired output format for each column in the result set.
    result_formats: Vec<PostgreSqlFormat>,
}

impl Portal {
    /// Constructs a new `Portal`.
    pub fn new(statement_name: String, stmt: Statement, result_formats: Vec<PostgreSqlFormat>) -> Self {
        Self {
            statement_name,
            stmt,
            result_formats,
        }
    }

    /// Returns the bound SQL statement.
    pub fn stmt(&self) -> &Statement {
        &self.stmt
    }
}
