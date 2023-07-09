use crate::{
    ConnectionSchema, GetGroupRespSchema, GetInterceptDataResp, HttpInterceptSchema, TrafficResp,
    TunStatusSchema,
};

#[tarpc::service]
pub trait ControlService {
    // Proxies
    async fn get_all_proxies() -> Vec<GetGroupRespSchema>;

    async fn get_proxy_group(group: String) -> Vec<GetGroupRespSchema>;

    async fn set_proxy_for(group: String, proxy: String) -> bool;

    async fn update_group_latency(group: String) -> bool;

    // Interceptions
    async fn get_all_interceptions() -> Vec<HttpInterceptSchema>;

    async fn get_range_interceptions(start: u32, end: Option<u32>) -> Vec<HttpInterceptSchema>;

    async fn get_intercepted_payload(id: u32) -> Option<GetInterceptDataResp>;

    // Connections
    async fn get_all_conns() -> Vec<ConnectionSchema>;

    async fn stop_all_conns();

    async fn stop_conn(id: u32) -> bool;

    // Temporary rules
    async fn add_temporary_rule(rule_literal: String) -> bool;

    async fn delete_temporary_rule(rule_literal_prefix: String) -> bool;

    async fn clear_temporary_rule();

    // General
    async fn get_tun() -> TunStatusSchema;

    async fn set_tun(enabled: TunStatusSchema) -> bool;

    async fn get_traffic() -> TrafficResp;

    async fn reload();

    // Streaming
    async fn request_traffic_stream(ctx_id: u64);

    async fn request_log_stream(ctx_id: u64);
}

#[tarpc::service]
// Used for streaming response from server
// Achieved by setting a listener in client side
// When such methods return invalid ctx_id, we can safely terminate posting.
pub trait ClientStreamService {
    async fn post_traffic(traffic: TrafficResp) -> u64;

    async fn post_log(log: String) -> u64;
}
