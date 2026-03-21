es_entity::entity_id! { UserId, AgentId }

macro_rules! impl_graphql_scalar {
    ($name:ident) => {
        #[async_graphql::Scalar]
        impl async_graphql::ScalarType for $name {
            fn parse(value: async_graphql::Value) -> async_graphql::InputValueResult<Self> {
                if let async_graphql::Value::String(s) = &value {
                    let uuid =
                        uuid::Uuid::parse_str(s).map_err(async_graphql::InputValueError::custom)?;
                    Ok(Self::from(uuid))
                } else {
                    Err(async_graphql::InputValueError::expected_type(value))
                }
            }

            fn to_value(&self) -> async_graphql::Value {
                async_graphql::Value::String(self.to_string())
            }
        }
    };
}

impl_graphql_scalar!(UserId);
impl_graphql_scalar!(AgentId);
