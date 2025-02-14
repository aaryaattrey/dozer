use std::{collections::HashMap, sync::Arc};

use dozer_core::{
    node::{PortHandle, Processor, ProcessorFactory},
    DEFAULT_PORT_HANDLE,
};
use dozer_sql_expression::{
    builder::ExpressionBuilder,
    execution::Expression,
    sqlparser::ast::{Expr, Ident, SelectItem},
};
use dozer_types::{
    errors::internal::BoxedError,
    types::{FieldDefinition, Schema},
};
use dozer_types::{models::udf_config::UdfConfig, tonic::async_trait};
use tokio::runtime::Runtime;

use crate::errors::PipelineError;

use super::processor::ProjectionProcessor;

#[derive(Debug)]
pub struct ProjectionProcessorFactory {
    select: Vec<SelectItem>,
    id: String,
    udfs: Vec<UdfConfig>,
    runtime: Arc<Runtime>,
}

impl ProjectionProcessorFactory {
    /// Creates a new [`ProjectionProcessorFactory`].
    pub fn _new(
        id: String,
        select: Vec<SelectItem>,
        udfs: Vec<UdfConfig>,
        runtime: Arc<Runtime>,
    ) -> Self {
        Self {
            select,
            id,
            udfs,
            runtime,
        }
    }
}

#[async_trait]
impl ProcessorFactory for ProjectionProcessorFactory {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn type_name(&self) -> String {
        "Projection".to_string()
    }

    fn get_input_ports(&self) -> Vec<PortHandle> {
        vec![DEFAULT_PORT_HANDLE]
    }

    fn get_output_ports(&self) -> Vec<PortHandle> {
        vec![DEFAULT_PORT_HANDLE]
    }

    async fn get_output_schema(
        &self,
        _output_port: &PortHandle,
        input_schemas: &HashMap<PortHandle, Schema>,
    ) -> Result<Schema, BoxedError> {
        let input_schema = input_schemas.get(&DEFAULT_PORT_HANDLE).unwrap();

        let mut select_expr: Vec<(String, Expression)> = vec![];
        for s in self.select.iter() {
            match s {
                SelectItem::Wildcard(_) => {
                    let fields: Vec<SelectItem> = input_schema
                        .fields
                        .iter()
                        .map(|col| {
                            SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(
                                col.to_owned().name,
                            )))
                        })
                        .collect();
                    for f in fields {
                        if let Ok(res) = parse_sql_select_item(
                            &f,
                            input_schema,
                            &self.udfs,
                            self.runtime.clone(),
                        )
                        .await
                        {
                            select_expr.push(res)
                        }
                    }
                }
                _ => {
                    if let Ok(res) =
                        parse_sql_select_item(s, input_schema, &self.udfs, self.runtime.clone())
                            .await
                    {
                        select_expr.push(res)
                    }
                }
            }
        }

        let mut output_schema = input_schema.clone();
        let mut fields = vec![];
        for e in select_expr.iter() {
            let field_name = e.0.clone();
            let field_type = e.1.get_type(input_schema)?;
            fields.push(FieldDefinition::new(
                field_name,
                field_type.return_type,
                field_type.nullable,
                field_type.source,
            ));
        }
        output_schema.fields = fields;

        Ok(output_schema)
    }

    async fn build(
        &self,
        input_schemas: HashMap<PortHandle, Schema>,
        _output_schemas: HashMap<PortHandle, Schema>,
        checkpoint_data: Option<Vec<u8>>,
    ) -> Result<Box<dyn Processor>, BoxedError> {
        let schema = match input_schemas.get(&DEFAULT_PORT_HANDLE) {
            Some(schema) => Ok(schema),
            None => Err(PipelineError::InvalidPortHandle(DEFAULT_PORT_HANDLE)),
        }?;

        let mut expressions = vec![];
        for select in &self.select {
            expressions.push(
                parse_sql_select_item(select, schema, &self.udfs, self.runtime.clone()).await?,
            );
        }
        Ok(Box::new(ProjectionProcessor::new(
            schema.clone(),
            expressions.into_iter().map(|e| e.1).collect(),
            checkpoint_data,
        )?))
    }
}

pub(crate) async fn parse_sql_select_item(
    sql: &SelectItem,
    schema: &Schema,
    udfs: &[UdfConfig],
    runtime: Arc<Runtime>,
) -> Result<(String, Expression), PipelineError> {
    match sql {
        SelectItem::UnnamedExpr(sql_expr) => {
            let expr = ExpressionBuilder::new(0, runtime)
                .parse_sql_expression(true, sql_expr, schema, udfs)
                .await?;
            Ok((sql_expr.to_string(), expr))
        }
        SelectItem::ExprWithAlias { expr, alias } => {
            let expr = ExpressionBuilder::new(0, runtime)
                .parse_sql_expression(true, expr, schema, udfs)
                .await?;
            Ok((alias.value.clone(), expr))
        }
        SelectItem::Wildcard(_) => Err(PipelineError::InvalidOperator("*".to_string())),
        SelectItem::QualifiedWildcard(ref object_name, ..) => {
            Err(PipelineError::InvalidOperator(object_name.to_string()))
        }
    }
}
