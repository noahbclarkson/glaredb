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

use crate::planner::{AsyncContextProvider, SqlQueryPlanner};

use async_recursion::async_recursion;
use datafusion::common::{DataFusionError, Result, ScalarValue};
use datafusion::logical_expr::{Expr, LogicalPlan, LogicalPlanBuilder};
use datafusion::sql::planner::PlannerContext;
use datafusion::sql::sqlparser::ast::{
    Expr as SQLExpr, Offset as SQLOffset, OrderByExpr, Query, Value,
};

use datafusion::sql::sqlparser::parser::ParserError::ParserError;

impl<'a, S: AsyncContextProvider> SqlQueryPlanner<'a, S> {
    /// Generate a logical plan from an SQL query
    pub async fn query_to_plan(&mut self, query: Query) -> Result<LogicalPlan> {
        self.query_to_plan_with_context(query, &mut PlannerContext::new())
            .await
    }

    pub async fn query_to_plan_with_context(
        &mut self,
        query: Query,
        planner_context: &mut PlannerContext,
    ) -> Result<LogicalPlan> {
        self.query_to_plan_with_schema(query, planner_context).await
    }

    /// Generate a logic plan from an SQL query.
    /// It's implementation of `subquery_to_plan` and `query_to_plan`.
    /// It shouldn't be invoked directly.
    #[async_recursion]
    pub(crate) async fn query_to_plan_with_schema(
        &mut self,
        query: Query,
        planner_context: &mut PlannerContext,
    ) -> Result<LogicalPlan> {
        let set_expr = query.body;
        if let Some(with) = query.with {
            // Process CTEs from top to bottom
            // do not allow self-references
            if with.recursive {
                return Err(DataFusionError::NotImplemented(
                    "Recursive CTEs are not supported".to_string(),
                ));
            }

            for cte in with.cte_tables {
                // A `WITH` block can't use the same name more than once
                let cte_name = self.normalizer.normalize(cte.alias.name.clone());
                if planner_context.contains_cte(&cte_name) {
                    return Err(DataFusionError::SQL(ParserError(format!(
                        "WITH query name {cte_name:?} specified more than once"
                    ))));
                }
                // create logical plan & pass backreferencing CTEs
                // CTE expr don't need extend outer_query_schema
                let logical_plan = self
                    .query_to_plan_with_context(*cte.query, &mut planner_context.clone())
                    .await?;

                // Each `WITH` block can change the column names in the last
                // projection (e.g. "WITH table(t1, t2) AS SELECT 1, 2").
                let logical_plan = self.apply_table_alias(logical_plan, cte.alias)?;

                planner_context.insert_cte(cte_name, logical_plan);
            }
        }
        let plan = self.set_expr_to_plan(*set_expr, planner_context).await?;
        let plan = self.order_by(plan, query.order_by, planner_context).await?;
        self.limit(plan, query.offset, query.limit).await
    }

    /// Wrap a plan in a limit
    async fn limit(
        &mut self,
        input: LogicalPlan,
        skip: Option<SQLOffset>,
        fetch: Option<SQLExpr>,
    ) -> Result<LogicalPlan> {
        if skip.is_none() && fetch.is_none() {
            return Ok(input);
        }

        let skip = match skip {
            Some(skip_expr) => match self
                .sql_to_expr(skip_expr.value, input.schema(), &mut PlannerContext::new())
                .await?
            {
                Expr::Literal(ScalarValue::Int64(Some(s))) => {
                    if s < 0 {
                        return Err(DataFusionError::Plan(format!(
                            "Offset must be >= 0, '{s}' was provided."
                        )));
                    }
                    Ok(s as usize)
                }
                _ => Err(DataFusionError::Plan(
                    "Unexpected expression in OFFSET clause".to_string(),
                )),
            }?,
            _ => 0,
        };

        let fetch = match fetch {
            Some(limit_expr) if limit_expr != SQLExpr::Value(Value::Null) => {
                let n = match self
                    .sql_to_expr(limit_expr, input.schema(), &mut PlannerContext::new())
                    .await?
                {
                    Expr::Literal(ScalarValue::Int64(Some(n))) if n >= 0 => Ok(n as usize),
                    _ => Err(DataFusionError::Plan(
                        "LIMIT must not be negative".to_string(),
                    )),
                }?;
                Some(n)
            }
            _ => None,
        };

        LogicalPlanBuilder::from(input).limit(skip, fetch)?.build()
    }

    /// Wrap the logical in a sort
    async fn order_by(
        &mut self,
        plan: LogicalPlan,
        order_by: Vec<OrderByExpr>,
        planner_context: &mut PlannerContext,
    ) -> Result<LogicalPlan> {
        if order_by.is_empty() {
            return Ok(plan);
        }

        let order_by_rex = self
            .order_by_to_sort_expr(&order_by, plan.schema(), planner_context)
            .await?;
        LogicalPlanBuilder::from(plan).sort(order_by_rex)?.build()
    }
}
