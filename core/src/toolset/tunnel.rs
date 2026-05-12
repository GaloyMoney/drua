#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSetScope {
    Tunnel {
        deployment_id: String,
        session_id: uuid::Uuid,
        route: TunnelRoute,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelRoute {
    Owned,
    Proxy,
}
